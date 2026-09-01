//! Native GUI shell: Welcome screen (project create/open) then a
//! three-panel layout (directory tree, tabbed editor, Claude panel) over a
//! `Project`, with a persisted dark/light theme. See
//! `docs/features/editor-shell.md` for the full behavioral spec.
//!
//! Non-rendering state/logic (tab management, the confirm-discard state
//! machine, the text-diff bridging egui's `TextEdit` to
//! `Buffer`'s offset-based `insert`/`delete`) lives in plain methods that
//! don't touch `egui::Context`, so it's unit-testable without a GUI
//! harness. Rendering (the `eframe::App::ui` body and its helpers) lives
//! in the child module `render` and is exempt from the coverage target —
//! it's thin, delegating every decision to the tested methods here.

use crate::cargo_panel::{CargoCommand, CargoPanel};
use crate::claude_panel::{ClaudeMessage, ClaudePanel};
use crate::claude_terminal::ClaudeTerminalPanel;
use crate::clone_panel::CloneState;
use crate::command::{self, Binding, CommandAction, KeyChord};
use crate::editor::{self, BlameAnnotation, EditorState};
use crate::file_structure;
use crate::files_search;
use crate::find_bar::FindBar;
use crate::git_panel::GitPanel;
use crate::keymap::{ImportReport, KeymapOverlay};
use crate::lsp_bridge::LspBridge;
use crate::nav_history::{NavHistory, NavLocation};
use crate::search_in_path_panel::PathSearchPanel;
use crate::search_panel::SearchPanel;
use crate::theme::Theme;
use crate::tree_scan::TreeScan;
use eframe::egui;
use ide_core::project_settings::{self, ProjectSettingsFile};
use ide_core::{
    editorconfig, replace_all, replace_one, syntax_for_path, Buffer, Charset, DirEntry,
    EditorConfig, EndOfLine, FileWatcher, IndentStyle, IndentUnit, LanguageConfig,
    PathSearchOptions, Project, ReplaceResult, SearchQuery, Selection, Selections, SyntaxRules,
    WatchEvent,
};
use ide_lsp::{
    position_is_within_interface, Diagnostic, DiagnosticSeverity, GotoKind, InlayHint, Location,
    LspRequest, Position, Symbol, SymbolKind,
};
use std::ops::Range;
use std::path::{Path, PathBuf};

mod menu;
mod render;

const LAST_PROJECT_STORAGE_KEY: &str = "ide_last_project";

/// Caps how many `open_tabs` entries `load_project_settings` restores.
/// `.ide/workspace.json` can arrive hand-edited or from a cloned
/// untrusted repository (`project-settings.md` §2.1's path-safety note);
/// without a cap, a crafted file listing many distinct real paths already
/// present in the repo drives `open_file`'s `O(n)` tab-dedup scan
/// `n` times, an `O(n²)` UI-thread hang on project open (hacker finding,
/// `docs/security-findings/rust-ui-dev-project-settings-2026-08-25.md`).
/// Comfortably above any doc example (a handful of tabs) or realistic
/// interactive workflow.
const MAX_RESTORED_TABS: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Editor,
    SourceControl,
}

impl ViewMode {
    fn toggled(self) -> Self {
        match self {
            ViewMode::Editor => ViewMode::SourceControl,
            ViewMode::SourceControl => ViewMode::Editor,
        }
    }
}

/// Which body the Claude rail's tab strip shows (`docs/features/
/// claude-terminal.md` §3.1/§6): the existing one-shot chat panel, or one
/// of `claude_terminals`' PTY tabs by index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeView {
    Chat,
    Terminal(usize),
}

/// The four tabs of the Search Everywhere popup (`docs/features/
/// search-everywhere.md` §2/§3.2), in this fixed declaration order --
/// `search_everywhere_switch_tab` cycles through them in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchEverywhereTab {
    Files,
    Symbols,
    Actions,
    Text,
}

/// One row of whatever the active Search Everywhere tab currently has to
/// show (§3.2) -- internal to `app`/`app::render`, never exposed further.
enum SearchEverywhereRow {
    File(ide_core::FuzzyFileMatch),
    Symbol(Symbol),
    Action(&'static command::Command),
    Text(ide_core::SearchMatch),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BottomView {
    Problems,
    CargoOutput,
    /// Find-usages results (doc §2.2/§3) -- selected via the bottom
    /// panel's three-way `selectable_label` row, same as the other two,
    /// rather than a binary toggle (there's no meaningful "next" view once
    /// there are three).
    Usages,
    /// Global search results (`docs/features/global-search-and-languages.md`
    /// §2.2/§3) -- extends the row above to four-way.
    Search,
}

/// Parameterizes `IdeApp::toggle_tool_window` / `is_tool_window_open`
/// (`fleet-shell.md` §2.5) -- not a `command::commands()` entry itself; the
/// five `ToggleXToolWindow` `CommandAction`s each map to one of these in
/// `run_command`'s match arm, `Bottom` handling three of them by also
/// switching `bottom_view` (`toggle_bottom_tool_window`, §3.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolWindow {
    Project,
    Claude,
    Bottom,
}

/// Three real states, deliberately not the four `docs/roadmap.md` §4.3
/// mentions -- see `IdeApp::smart_mode_state`'s doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmartModeState {
    Off,
    On,
    Error,
}

pub struct Tab {
    pub buffer: Buffer,
    pub title: String,
    /// Per-tab editor widget state: caret stickiness, galley cache, scroll
    /// request. The buffer itself holds the text, cursors and undo history.
    pub editor: EditorState,
    /// Kept in sync with `LspBridge::diagnostics`' entry for this tab's
    /// path every frame (see `IdeApp::sync_tab_diagnostics`) -- backs the
    /// inline squiggle overlay in the editor.
    pub diagnostics: Vec<Diagnostic>,
    /// Fixed at tab creation from the buffer's path (filename first, then
    /// extension -- see `ide_core::syntax_for_path`) -- a per-file concern,
    /// deliberately independent of `active_language`'s per-project LSP
    /// detection (docs/features/syntax-highlighting.md §1). `None` for an
    /// untitled buffer or a path no built-in language claims; never
    /// recomputed on a later Save As (known v1 limitation, see the doc's
    /// §2.2). Since A2 the tokens themselves live in the buffer, so this is
    /// kept only as the record of what was installed -- read by tests, and
    /// by the Save-As re-detect that limitation still owes.
    #[allow(dead_code)]
    syntax: Option<&'static SyntaxRules>,
    /// The `.editorconfig` properties in force for this tab
    /// (`line-commands-and-editorconfig.md` §3.6) -- `EditorConfig::default()`
    /// (every field `None`) for an untitled buffer or one outside any open
    /// project, resolved from the real path otherwise. `editor`'s indent is
    /// derived from this at the same time; kept separately because
    /// `save_edit`/`save_charset` need the untranslated config, not the
    /// `IndentUnit` it was mapped onto.
    config: EditorConfig,
    /// Whether `save_tab_with_config` has already surfaced the "saved as
    /// UTF-8" notice for this tab's `config.charset` (§3.6's charset rule) --
    /// reset by `apply_editor_config`, since Save As can resolve a
    /// different `.editorconfig` with a different (or no) charset.
    charset_notice_shown: bool,
    /// Set by `poll_watcher` (`file-watcher.md` §3.4/§3.5) when this tab's
    /// path changed or was removed on disk. Cleared by Reload, Keep Mine,
    /// dismissing a Deleted notice, or the tab closing. `None` for an
    /// untitled tab -- it has no path, so nothing on disk can change under
    /// it.
    pub external_change: Option<ExternalChange>,
    /// This tab's own find/replace session (`in-buffer-find-replace.md`
    /// §2.2) -- one per `Tab`, not shared app-wide, since "3 of 17" is
    /// meaningless once you're not looking at the buffer it was computed
    /// against.
    find: FindBar,
    /// Blame annotations for this tab, `None` when off (the ordinary Rust
    /// idiom for "not computed/not active") -- per-tab, session-only
    /// toggle state (`docs/features/git-branches-and-blame.md` §2.2.3),
    /// populated by `toggle_blame_annotations` and refreshed on Save and
    /// on Reload, the same triggers the git gutter's own marks already
    /// key off. A cache invalidated wholesale on toggle/reload, not a
    /// per-line set mutated incrementally -- a different shape than
    /// `editor.folded`'s live toggle set, though the closest existing
    /// precedent for "per-tab, UI-only, off-by-default state" all the
    /// same.
    pub blame: Option<Vec<BlameAnnotation>>,
}

/// `file-watcher.md` §2.2/§3.4/§3.5.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalChange {
    /// Offers Reload / Keep Mine.
    Modified,
    /// Offers an acknowledgement (and, if the tab has unsaved content, an
    /// implicit "save to recreate the file" affordance -- `save_active`
    /// already does that: `Buffer::save`/`save_with` create the file if it
    /// doesn't exist).
    Deleted,
}

impl Tab {
    fn syntax_for_buffer(buffer: &Buffer) -> Option<&'static SyntaxRules> {
        buffer.path().and_then(syntax_for_path)
    }

    fn from_buffer(buffer: Buffer) -> Self {
        let title = buffer
            .path()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".to_string());
        let syntax = Self::syntax_for_buffer(&buffer);
        let mut buffer = buffer;
        buffer.set_syntax(syntax);
        Self {
            buffer,
            title,
            editor: EditorState::default(),
            diagnostics: Vec::new(),
            syntax,
            config: EditorConfig::default(),
            charset_notice_shown: false,
            external_change: None,
            find: FindBar::default(),
            blame: None,
        }
    }

    fn untitled(title: String) -> Self {
        Self {
            buffer: Buffer::untitled(),
            title,
            editor: EditorState::default(),
            diagnostics: Vec::new(),
            syntax: None,
            config: EditorConfig::default(),
            charset_notice_shown: false,
            external_change: None,
            find: FindBar::default(),
            blame: None,
        }
    }

    /// Applies `config` to this tab: stores it for the save-time properties
    /// (§3.6) and maps `indent_style`/`indent_size` onto the editor's
    /// `IndentUnit` -- only the fields the config actually set, since a
    /// `None` here must never override `IndentUnit::default()`'s own choice.
    fn apply_editor_config(&mut self, config: EditorConfig) {
        let mut unit = IndentUnit::default();
        if let Some(style) = config.indent_style {
            unit.style = style;
        }
        if let Some(width) = config.indent_size {
            unit.width = width;
        }
        self.editor.set_indent(unit);
        self.config = config;
        self.charset_notice_shown = false;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingConfirm {
    CloseTab(usize),
    Quit,
}

/// `⇧F6`'s inline popup (`docs/features/rename-refactoring.md` §2.3).
struct RenamePopup {
    path: PathBuf,
    /// The caret position sent as the rename anchor.
    position: Position,
    original_name: String,
    /// Editable text the popup's `egui::TextEdit` widget binds to directly
    /// (same "render code mutates the field" convention the find bar's
    /// query already uses) -- pre-filled with `original_name`.
    input: String,
}

/// The "Refactor Preview" dialog's state (`docs/features/refactor-this.md`
/// §2.2, §3.3).
struct RefactorPreview {
    what: String,
    edit: ide_lsp::WorkspaceEdit,
    /// One entry per `FileEdit` in `edit.edits`, same order. `None` means
    /// the diff itself couldn't be computed (the file couldn't be read,
    /// or `apply_transaction` rejected an out-of-range edit) -- the row
    /// still renders (path + "diff unavailable"), it is not dropped from
    /// the list.
    diffs: Vec<Option<ide_core::FileDiff>>,
}

/// The "Replace in Path Preview" window's state
/// (`docs/features/search-in-path-v2.md` §2.2/§3.3) -- same shape as
/// `RefactorPreview` above, at the `ide_core::WorkspaceEdit` level instead
/// of `ide_lsp::WorkspaceEdit`, since Replace in Path's edits never touch
/// LSP at all.
struct ReplaceInPathPreview {
    edit: ide_core::WorkspaceEdit,
    /// One entry per `FileEdit` in `edit.edits`, same order and same
    /// `None`-means-diff-unavailable convention as `RefactorPreview::diffs`.
    diffs: Vec<Option<ide_core::FileDiff>>,
}

/// The five direct refactor commands' shared entry point
/// (`docs/features/refactor-this.md` §2.2, §3.2) -- a closed enum rather
/// than five near-identical methods, since `trigger_direct_refactor`'s
/// body is otherwise byte-for-byte the same for all five.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DirectRefactorKind {
    ExtractVariable,
    ExtractMethod,
    ExtractConstant,
    ExtractField,
    Inline,
}

impl DirectRefactorKind {
    fn name(self) -> &'static str {
        match self {
            Self::ExtractVariable => "Extract Variable",
            Self::ExtractMethod => "Extract Method",
            Self::ExtractConstant => "Extract Constant",
            Self::ExtractField => "Extract Field",
            Self::Inline => "Inline",
        }
    }

    /// `kind` prefix + `title` substrings (any match, case-insensitive)
    /// per `docs/features/refactor-this.md` §3.2's table.
    fn matches(self, action: &ide_lsp::CodeAction) -> bool {
        let Some(kind) = action.kind.as_deref() else {
            return false;
        };
        let title = action.title.to_lowercase();
        match self {
            Self::ExtractVariable => {
                kind.starts_with("refactor.extract") && title.contains("variable")
            }
            Self::ExtractMethod => {
                kind.starts_with("refactor.extract")
                    && (title.contains("function") || title.contains("method"))
            }
            Self::ExtractConstant => {
                kind.starts_with("refactor.extract") && title.contains("constant")
            }
            Self::ExtractField => kind.starts_with("refactor.extract") && title.contains("field"),
            Self::Inline => kind.starts_with("refactor.inline"),
        }
    }
}

/// The three direct-invoke Generate-family commands' shared entry point
/// (`docs/features/code-generation.md` §2.2, §3.2/§3.3) -- a sibling to
/// `DirectRefactorKind`, not a variant added to it, since its `matches`
/// heuristic is a genuinely different shape (kind-equals-empty-string-or-
/// quickfix plus title, not `starts_with` on a `refactor.*`/`quickfix`
/// prefix alone). The Generate menu's own filter (§3.1) is simpler still
/// -- kind-equals-empty-string alone, no title check -- so it isn't a
/// fourth variant here either.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DirectGenerateKind {
    ImplementMethods,
    OverrideMethods,
    CreateTest,
}

impl DirectGenerateKind {
    fn name(self) -> &'static str {
        match self {
            Self::ImplementMethods => "Implement Methods",
            Self::OverrideMethods => "Override Methods",
            Self::CreateTest => "Create Test",
        }
    }

    /// rust-analyzer's "Implement missing members"/"Implement default
    /// members" are both `quickfix`-kind (§1) -- title is the only
    /// reliable discriminator between them, so `ImplementMethods`/
    /// `OverrideMethods` each require a `quickfix`-kind action whose title
    /// contains their own exact rust-analyzer title string. `CreateTest`
    /// matches on title alone, kind-agnostic -- no rust-analyzer assist
    /// produces one today (§1), so this is permissive on purpose, the
    /// same "register anyway even if currently unsatisfiable" precedent
    /// `DirectRefactorKind::ExtractField` already sets, in case a
    /// configured server (or a future rust-analyzer) ever does.
    fn matches(self, action: &ide_lsp::CodeAction) -> bool {
        let title = action.title.to_lowercase();
        match self {
            Self::ImplementMethods => {
                action.kind.as_deref() == Some("quickfix")
                    && title.contains("implement missing members")
            }
            Self::OverrideMethods => {
                action.kind.as_deref() == Some("quickfix")
                    && title.contains("implement default members")
            }
            Self::CreateTest => title.contains("test"),
        }
    }
}

/// Which follow-up `poll_tree_scan` should run once the in-flight scan
/// completes (`async-tree-scan.md` §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TreeScanKind {
    Load,
    Refresh,
}

/// Everything this feature (`project-settings.md` §2.2) moves out of the
/// old global `eframe::Storage` keys and into `.ide/preferences.json`.
/// Not `#[derive(Default)]`: the fallback values must match exactly what
/// `IdeApp::new` used to fall back to when a global key was absent
/// (`Theme::Dark`, `false`, ...), so this has its own explicit `impl
/// Default`. `#[serde(default)]` at the container level means a
/// preferences file written by an older build (missing a field this
/// build added) fills that field from this same `Default` impl instead
/// of failing to deserialize (§3.4).
/// Caps how many entries `custom_languages`/`dismissed_language_suggestions`
/// can deserialize into from `.ide/preferences.json` -- the same
/// untrusted-file class `LanguageConfig.args`/`extra_extensions`'s own
/// bounded decode already guards (`crates/core/src/language.rs`'s
/// `MAX_LANGUAGE_CONFIG_LIST_LEN`), applied here because this is the
/// concrete site that made an unbounded `custom_languages` array a
/// live, measured problem: `ide_core::detect_active_languages` walks the
/// whole project tree once per entry, so a huge array here means a huge,
/// repeated, main-thread stall on every project load/tree-refresh/
/// language-settings save (`docs/security-findings/
/// rust-ui-dev-multi-language-projects-2026-08-28.md`, finding 1).
/// Comfortably above any real user's language count (this project itself
/// ships 14 built-in markers) so it never truncates legitimate use.
const MAX_CUSTOM_LANGUAGES: usize = 64;
const MAX_DISMISSED_LANGUAGE_SUGGESTIONS: usize = 256;
/// `docs/features/recent-files.md` §2.2 -- matches `tui-recent-files-and-
/// bookmarks.md`'s own cap for the identical MRU-list shape.
const MAX_RECENT_FILES: usize = 20;
/// Display cap only, not a persisted cap (`recent-files.md` §1.1) --
/// `NavHistory`'s own entry list is unbounded and session-only.
const MAX_RECENT_LOCATIONS_SHOWN: usize = 50;

fn deserialize_bounded_custom_languages<'de, D>(
    deserializer: D,
) -> Result<Vec<LanguageConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct BoundedVisitor;

    impl<'de> serde::de::Visitor<'de> for BoundedVisitor {
        type Value = Vec<LanguageConfig>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("an array of language configs")
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let mut items = Vec::new();
            while items.len() < MAX_CUSTOM_LANGUAGES {
                match seq.next_element::<LanguageConfig>()? {
                    Some(item) => items.push(item),
                    None => return Ok(items),
                }
            }
            while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
            Ok(items)
        }
    }

    deserializer.deserialize_seq(BoundedVisitor)
}

fn deserialize_bounded_dismissed_suggestions<'de, D>(
    deserializer: D,
) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct BoundedVisitor;

    impl<'de> serde::de::Visitor<'de> for BoundedVisitor {
        type Value = Vec<String>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("an array of strings")
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let mut items = Vec::new();
            while items.len() < MAX_DISMISSED_LANGUAGE_SUGGESTIONS {
                match seq.next_element::<String>()? {
                    Some(item) => items.push(item),
                    None => return Ok(items),
                }
            }
            while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
            Ok(items)
        }
    }

    deserializer.deserialize_seq(BoundedVisitor)
}

/// `recent-files.md` §2.2 -- same bounded-`Vec`-deserialize pattern as
/// `deserialize_bounded_custom_languages`/`deserialize_bounded_dismissed_
/// suggestions`; `workspace.json` is untrusted, same as `preferences.json`.
fn deserialize_bounded_recent_files<'de, D>(deserializer: D) -> Result<Vec<PathBuf>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct BoundedVisitor;

    impl<'de> serde::de::Visitor<'de> for BoundedVisitor {
        type Value = Vec<PathBuf>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("an array of paths")
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let mut items = Vec::new();
            while items.len() < MAX_RECENT_FILES {
                match seq.next_element::<PathBuf>()? {
                    Some(item) => items.push(item),
                    None => return Ok(items),
                }
            }
            while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
            Ok(items)
        }
    }

    deserializer.deserialize_seq(BoundedVisitor)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct ProjectPreferences {
    theme: Theme,
    #[serde(deserialize_with = "deserialize_bounded_custom_languages")]
    custom_languages: Vec<LanguageConfig>,
    keymap: KeymapOverlay,
    format_on_save: bool,
    /// `LanguageSuggestion::marker_file` values the user has dismissed for
    /// this project (`docs/features/language-auto-detect.md` §3.3).
    /// `#[serde(default)]` at the container level means an older
    /// `preferences.json` missing this field just starts with an empty
    /// list -- no migration needed.
    #[serde(deserialize_with = "deserialize_bounded_dismissed_suggestions")]
    dismissed_language_suggestions: Vec<String>,
}

impl Default for ProjectPreferences {
    fn default() -> Self {
        Self {
            theme: Theme::Dark,
            custom_languages: Vec::new(),
            keymap: KeymapOverlay::default(),
            format_on_save: false,
            dismissed_language_suggestions: Vec::new(),
        }
    }
}

/// `.ide/workspace.json`'s shape (`project-settings.md` §2.2). Paths are
/// relative to the project root (portable if the directory ever moves;
/// also just less noisy to read by hand than an absolute path would be).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct WorkspaceState {
    open_tabs: Vec<OpenTabState>,
    /// The relative path of the tab that was active, not an index --
    /// indices don't survive a restore where some remembered files are
    /// missing (§3.3).
    active_path: Option<PathBuf>,
    /// Root-relative paths, most-recently-used first, deduplicated
    /// (`recent-files.md` §2.2) -- same on-disk convention `OpenTabState.
    /// path` already uses. `#[serde(default)]` (from the container-level
    /// attribute above) so an older `workspace.json` without this field
    /// just starts empty.
    #[serde(deserialize_with = "deserialize_bounded_recent_files")]
    recent_files: Vec<PathBuf>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct OpenTabState {
    #[serde(default)]
    path: PathBuf,
    /// Primary cursor's byte offset only -- not multi-cursor state, not
    /// fold state. A deliberate v1 cut (doc §2.2). Clamped to the
    /// reopened buffer's length on restore in case the file shrank since
    /// the offset was saved.
    #[serde(default)]
    cursor_offset: usize,
}

pub struct IdeApp {
    theme: Theme,
    view_mode: ViewMode,
    bottom_view: BottomView,
    project: Option<Project>,
    tree: Option<DirEntry>,
    tree_scan: TreeScan,
    pending_tree_scan_kind: Option<TreeScanKind>,
    tabs: Vec<Tab>,
    active_tab: Option<usize>,
    untitled_count: usize,
    pending_confirm: Option<PendingConfirm>,
    should_quit: bool,
    error: Option<String>,
    pending_create_parent: Option<PathBuf>,
    create_project_name: String,
    claude: ClaudePanel,
    claude_terminals: ClaudeTerminalPanel,
    claude_view: ClaudeView,
    git: GitPanel,
    lsp: LspBridge,
    cargo: CargoPanel,
    clone: CloneState,
    /// `None` when no project is open. Replaced (dropping the old one)
    /// every time `load_project` runs (`file-watcher.md` §3.6).
    watcher: Option<FileWatcher>,
    /// Byte offset into the about-to-be-active tab's text to best-effort
    /// place the cursor at, set by `open_diagnostic` (doc §3's "Problems
    /// panel row click" behaviour) and consumed by rendering on the next
    /// frame the tab is drawn.
    pending_cursor_offset: Option<usize>,
    /// The active tab's current cursor byte offset, refreshed every frame
    /// right after the editor's `TextEdit` renders (doc §2.2) -- `find_usages`
    /// reads this rather than tracking cursor state itself, since egui's
    /// `TextEdit` is the only thing that actually knows where the cursor is.
    active_cursor_offset: Option<usize>,
    /// Byte range of the identifier the pointer was over while `Cmd`
    /// (`Ctrl` off macOS) was held, as of the previous frame — the editor's
    /// `TextEdit` layouter runs before this frame's pointer position is
    /// known, so the underline necessarily lags by one frame (which is
    /// invisible: egui repaints on pointer motion). `None` whenever the
    /// modifier is up or the pointer isn't on an identifier.
    hover_link: Option<Range<usize>>,
    /// Whether the floating Usages window is up. Set by `Cmd+B` and
    /// `Cmd+Click`, which deliberately don't touch `bottom_view` — the
    /// bottom panel's Usages tab stays available for browsing the same
    /// results, but the navigation gestures get a popup instead
    /// (`docs/features/richer-highlighting-and-usages-popup.md` §3).
    show_usages_popup: bool,
    /// Which of the three `Goto` queries is the most recently *triggered*
    /// one -- set by each `trigger_go_to_*` method before it calls the
    /// matching `LspBridge` method; read by `render_goto_popup` for its
    /// title/empty-state text and `goto_action_label`
    /// (`docs/features/goto-definition.md` §2.2).
    goto_action: Option<GotoKind>,
    /// Mirrors `show_usages_popup`, but only `handle_goto_response` ever
    /// sets it `true` -- never a trigger method directly, since the Go to
    /// popup only opens once the answer is already known (zero or more
    /// than one result), never while a query is in flight (§3.3/§4).
    show_goto_popup: bool,
    /// The `(path, position)` a `Cmd+Click`/`Cmd+B` press actually fired
    /// from -- remembered so `handle_interface_check_response` knows where
    /// to re-query `Implementation` from if the resolved declaration turns
    /// out to need one. `None` whenever no such check is pending
    /// (`docs/features/goto-declaration-interface-redirect.md` §2.2).
    goto_declaration_origin: Option<(PathBuf, Position)>,
    /// The single `Goto` result `handle_goto_response` is holding while it
    /// waits on a `DocumentSymbol` response (for that result's own file)
    /// to decide whether to jump to it directly or redirect to
    /// `Implementation` from `goto_declaration_origin` instead. `None`
    /// whenever no check is pending.
    pending_interface_check: Option<Location>,
    /// Whether the Quick Documentation popup is open. No separate "which
    /// query" tag like `goto_action`: there's only one kind of hover query,
    /// so the popup's title is a fixed `"Documentation"`
    /// (`docs/features/inlay-hints-and-hover.md` §2.2).
    show_hover_popup: bool,
    /// The `(path, position)` `sync_document_highlights` most recently
    /// fired a `DocumentHighlight` query for -- lets it tell "the caret
    /// moved to somewhere new" apart from "still sitting where it was last
    /// frame" without re-querying every single frame (§3.4).
    last_highlighted_target: Option<(PathBuf, Position)>,
    /// Whether the code actions popup (`⌥↩`) is open
    /// (`docs/features/code-actions.md` §2.3) -- mirrors `show_hover_popup`.
    show_code_actions_popup: bool,
    /// The `(path, position)` `sync_code_actions` most recently fired a
    /// `CodeAction` query for -- mirrors `last_highlighted_target`, drives
    /// `sync_code_actions`'s own "did the target change" check the same
    /// way.
    last_code_actions_target: Option<(PathBuf, Position)>,
    /// Whether the "Refactor This" (`⌃T`) popup is open
    /// (`docs/features/refactor-this.md` §2.2) -- mirrors
    /// `show_code_actions_popup`, but the popup it opens shows only
    /// `lsp.code_actions` entries whose `kind` starts with `"refactor"`.
    show_refactor_menu_popup: bool,
    /// Whether the Generate (`⌘N`/`Alt+Insert`) popup is open
    /// (`docs/features/code-generation.md` §2.2, §3.1) -- mirrors
    /// `show_refactor_menu_popup`, but the popup it opens shows only
    /// `lsp.code_actions` entries whose `kind` is the empty string (rust-
    /// analyzer's `AssistKind::Generate` mapping, §1) rather than a
    /// `starts_with` prefix match.
    show_generate_menu_popup: bool,
    /// In-memory MRU list of recently opened/focused files, most-recent-
    /// first, deduplicated -- persisted via `WorkspaceState.recent_files`
    /// (`docs/features/recent-files.md` §2.2/§2.4). Absolute canonical
    /// paths (unlike the persisted, root-relative form).
    recent_files: Vec<PathBuf>,
    /// Whether the Recent Files (`⌘E`) popup is open (`recent-files.md`
    /// §2.3).
    recent_files_open: bool,
    recent_files_query: String,
    recent_files_selected: usize,
    /// Mirrors `pending_search_everywhere_focus` -- consumed (`mem::
    /// take`) by `render_recent_files_popup` on the frame it's `true`, to
    /// focus the query text box the moment the popup opens.
    pending_recent_files_focus: bool,
    /// Whether the Recent Locations (`⌘⇧E`) popup is open. No query field
    /// -- this popup has no text input (`recent-files.md` §1.1).
    recent_locations_open: bool,
    recent_locations_selected: usize,
    /// Set immediately before this phase's code sends
    /// `LspRequest::ApplyCodeAction`, cleared unconditionally the next
    /// time `handle_workspace_edit_ready` observes a real
    /// `workspace_edit_ready` event (§3.4) -- routes that one specific
    /// `WorkspaceEditReady` into `show_refactor_preview` instead of
    /// `handle_workspace_edit_ready`'s existing immediate-apply body.
    /// `⌥↩`'s own direct `select_code_action` calls leave this `false`.
    via_refactor_preview: bool,
    /// Awaiting the user's Apply/Cancel on a refactor's diff'd preview
    /// (`docs/features/refactor-this.md` §2.2, §3.3/§3.5) -- presence is
    /// visibility, same convention `pending_rename_preview` uses.
    pending_refactor_preview: Option<RefactorPreview>,
    /// The active tab's gutter marks -- recomputed every frame by
    /// `sync_git_gutter` (`docs/features/editor-git-gutter.md` §2.4, §3.1).
    /// Empty whenever the active tab has unsaved edits, has no path, or
    /// there is no active tab at all.
    git_gutter: Vec<crate::editor::GutterMark>,
    /// The path `git_gutter` answers, if any -- `None` exactly when
    /// `git_gutter` is empty for one of the reasons above.
    git_gutter_path: Option<PathBuf>,
    /// The buffer line a gutter-mark click landed on, while its "Revert
    /// Hunk"/"Show Diff" popup is open. `None` when closed.
    git_gutter_popup_line: Option<usize>,
    /// The commit id whose detail the blame popup is showing
    /// (`docs/features/git-branches-and-blame.md` §2.2.3) -- looked up
    /// live via `GitPanel::commit_detail` each frame the popup is open
    /// (a single-commit lookup, cheap enough not to cache), the same
    /// "presence is visibility" convention every other popup field here
    /// already follows.
    blame_popup_commit_id: Option<String>,
    /// Set by `open_find_bar` when the active tab's bar just transitioned
    /// from closed to open, or from find-only to with-replace -- consumed
    /// by `render_find_bar` on the next frame to focus the query field
    /// (doc §3.1), the same "mutate state now, rendering consumes a pending
    /// flag next frame" shape `pending_cursor_offset` already uses.
    pending_find_focus: bool,
    search: PathSearchPanel,
    search_query: String,
    /// Regex/whole-word/case-sensitivity, include/exclude globs, and
    /// `.gitignore` respect -- fed to both `search_tree_advanced` and
    /// `replace_in_path` (`docs/features/search-in-path-v2.md` §2.2/§3.6).
    /// `respect_gitignore: true` by default, everything else empty/default.
    /// `include`/`exclude` are only ever written by `run_search`/
    /// `run_replace_preview` parsing `search_include_text`/
    /// `search_exclude_text` at submit time -- never derived from them on
    /// every frame, which would eat a trailing comma/separator the user
    /// just typed before the next keystroke lands (rev finding, fix round
    /// 1: a per-frame `Vec<String>` join/split round-trip through
    /// `split_glob_list` silently drops the empty token after a trailing
    /// separator, so `render_search_panel` would revert "*.rs, " to
    /// "*.rs" on the very next repaint -- cursor blink alone triggers one).
    search_options: PathSearchOptions,
    /// Raw text the user is typing for `search_options.include`/`exclude`
    /// -- kept separate from the parsed `Vec<String>` for exactly the
    /// reason in the comment above; parsed via `split_glob_list` only when
    /// `run_search`/`run_replace_preview` actually build a request.
    search_include_text: String,
    search_exclude_text: String,
    search_replacement: String,
    /// Whether the Replacement field/Preview button are showing (doc
    /// §2.3/§3.3) -- mirrors `FindBar::replace_open`'s convention: set by
    /// `trigger_replace_in_path`, never turned back off on its own.
    search_replace_open: bool,
    /// Awaiting the user's Apply/Cancel on a Replace in Path diff'd preview
    /// -- presence is visibility, same convention `pending_refactor_preview`
    /// uses.
    pending_replace_in_path_preview: Option<ReplaceInPathPreview>,
    /// User-added language configs, persisted per-project in
    /// `.ide/preferences.json` the same way `theme` is
    /// (`project-settings.md` §2.2, superseding the old global
    /// `eframe::Storage` key `global-search-and-languages.md` §2.2
    /// originally used). Never includes `LanguageConfig::rust()` -- that's
    /// a permanent, separate special case `detect_language` always checks
    /// first (§1/§4).
    custom_languages: Vec<LanguageConfig>,
    /// Recomputed via `ide_core::detect_language` everywhere `tree` is
    /// recomputed (`load_project`/`refresh_tree`/`resync_active_languages`)
    /// -- replaces the direct `Cargo.toml` checks those methods used
    /// before this feature (§6). Every language currently active for the
    /// open project, Rust first if present -- no longer exclusive
    /// (`docs/features/multi-language-projects.md` §2.3); drives
    /// `restart_lsp`'s targets and the "Restart Language Server" button's
    /// gate; `is_rust_project()` stays its own separate check (§3).
    active_languages: Vec<LanguageConfig>,
    new_language_name: String,
    new_language_extension: String,
    new_language_command: String,
    new_language_args: String,
    language_settings_error: Option<String>,
    show_language_settings: bool,
    /// Mirrors `ProjectPreferences::dismissed_language_suggestions`,
    /// synced in `load_project_settings`/`flush_project_settings` exactly
    /// like `custom_languages` above
    /// (`docs/features/language-auto-detect.md` §2.2).
    dismissed_language_suggestions: Vec<String>,
    /// Suggestions currently awaiting a user answer, filtered and ordered
    /// by `refresh_language_suggestions`. The popup always shows
    /// `.first()`; resolving one (Enable or Dismiss) removes it from this
    /// list, revealing the next if more than one marker matched
    /// (`docs/features/language-auto-detect.md` §2.2/§3.4).
    pending_language_suggestions: Vec<ide_core::LanguageSuggestion>,
    /// `⌘⇧A` ("Find Action"). `command-palette.md` §2.2/§3.2.
    command_palette_open: bool,
    command_palette_query: String,
    command_palette_selected: usize,
    /// Deferred-focus flag for the palette's query field, same
    /// "mutate state now, rendering consumes a pending flag next frame"
    /// shape `pending_find_focus` already uses.
    pending_command_palette_focus: bool,
    /// User overrides + preset scheme layered over `command::commands()`'s
    /// defaults, persisted per-project the same way `theme`/
    /// `custom_languages` are (`keymap.md` §2.6, `project-settings.md`
    /// §2.2).
    keymap: KeymapOverlay,
    show_keymap_settings: bool,
    keymap_search: String,
    /// Id of the command currently being (re)bound, while the Keymap
    /// window is waiting for the next keypress (`keymap.md` §2.6/§3.5).
    keymap_capture_target: Option<&'static str>,
    /// A captured chord awaiting Confirm/Cancel, plus the ids it would
    /// conflict with (`keymap.md` §2.5's `conflicts`).
    keymap_capture_pending: Option<(KeyChord, Vec<&'static str>)>,
    keymap_import_error: Option<String>,
    /// Back/forward stack (`fleet-shell.md` §2.4/§3.2).
    nav: NavHistory,
    show_project_tool_window: bool,
    show_claude_tool_window: bool,
    show_bottom_tool_window: bool,
    /// Session-only, like the four fields above -- none of these six persist
    /// via `eframe::Storage` (§2.5/§4.3).
    zen_mode: bool,
    /// `⇧⇧` / `⌘⇧O` / `⌘O` / `⌘⌥O` (`docs/features/search-everywhere.md`
    /// §3.2).
    search_everywhere_open: bool,
    search_everywhere_tab: SearchEverywhereTab,
    search_everywhere_query: String,
    search_everywhere_selected: usize,
    pending_search_everywhere_focus: bool,
    /// Set by Go to Class to restrict the Symbols tab's results to
    /// class-like kinds (`Class`/`Struct`/`Interface`/`Enum`) until the
    /// popup closes (§3.5).
    search_everywhere_class_filter: bool,
    /// The query `query_workspace_symbols` was last sent for -- avoids
    /// re-sending an identical request every frame the popup is open
    /// (§3.2/§3.3), same "track what was last asked for" shape
    /// `last_code_actions_target` already established.
    last_workspace_symbol_query: Option<String>,
    /// The path `request_document_symbols` was last *sent* for, as opposed
    /// to `LspBridge::document_symbols_path`, which only updates once a
    /// *response* arrives -- without this the empty-query Symbols branch
    /// would re-send a fresh request every frame while a response is still
    /// in flight (§3.2).
    document_symbols_requested_for: Option<PathBuf>,
    /// `⌘F12` File Structure popup (`file-structure-and-breadcrumbs.md`
    /// §2.3/§3.2) -- same open/query/selected/focus shape as Search
    /// Everywhere, scoped to the active file's own outline instead of a
    /// workspace-wide query.
    file_structure_open: bool,
    file_structure_query: String,
    file_structure_selected: usize,
    pending_file_structure_focus: bool,
    /// The query `search_everywhere_files`/`search_everywhere_text` was
    /// last actually *run* for -- `None` while no run has started yet, or
    /// while one is still in flight for an older query (only updated once
    /// `run` actually launches a search, not merely attempted -- see
    /// `sync_search_everywhere`). Same "track what was last sent, not what
    /// was last received" shape as `last_workspace_symbol_query`, needed
    /// here because `FilesSearchPanel`/`SearchPanel` don't themselves
    /// remember which query they last ran.
    last_files_query: Option<String>,
    last_text_query: Option<String>,
    /// Text tab: a second, independent `SearchPanel` instance, not shared
    /// with the Find in Path tool window's own (§3.2).
    search_everywhere_text: SearchPanel,
    /// Files tab (§3.2).
    search_everywhere_files: files_search::FilesSearchPanel,
    show_go_to_line: bool,
    go_to_line_input: String,
    /// Deferred-focus flag for the Go to Line dialog's text field, same
    /// "mutate state now, rendering consumes a pending flag next frame"
    /// shape `pending_find_focus`/`pending_command_palette_focus` already
    /// use (§3.5).
    pending_go_to_line_focus: bool,
    /// Fed Shift's rising/falling edge every frame (§3.5).
    search_everywhere_double_tap: editor::double_tap::DoubleTap,
    search_everywhere_shift_down: bool,
    /// Per-project toggle, persisted in `.ide/preferences.json`
    /// (`docs/features/formatting.md` §2.3, `project-settings.md` §2.2).
    /// When on, a successful `⌘S` also fires a follow-up Reformat Code
    /// request for that file.
    format_on_save: bool,
    /// Set right after `try_save_active`'s synchronous save succeeds and
    /// fires the format-on-save follow-up request; cleared once
    /// `handle_format_ready` processes a response whose path matches,
    /// regardless of outcome (`docs/features/formatting.md` §2.3, §3.4).
    format_on_save_target: Option<PathBuf>,
    /// Presence *is* visibility, no separate `show_*` bool (unlike
    /// `show_hover_popup`'s pair with `lsp.hover`: that content is worth
    /// keeping around after the popup closes for a possible reopen; a
    /// `RenamePopup` has no such reason to survive its own close)
    /// (`docs/features/rename-refactoring.md` §2.3).
    rename_popup: Option<RenamePopup>,
    /// Deferred-focus flag for the rename popup's input field, same
    /// "mutate state now, rendering consumes a pending flag next frame"
    /// shape `pending_find_focus` already uses.
    pending_rename_focus: bool,
    /// `(edit, new_name)` awaiting the user's Apply/Cancel -- same
    /// "presence is visibility" reasoning as `rename_popup`.
    pending_rename_preview: Option<(ide_lsp::WorkspaceEdit, String)>,
}

impl IdeApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // Theme/custom_languages/keymap/format_on_save are no longer read
        // from global `eframe::Storage` (`project-settings.md` §4): they
        // start at the same hardcoded defaults the welcome screen (no
        // project open) always shows, and `load_project_settings` below
        // overwrites them with the restored project's own values, if any.
        let theme = Theme::Dark;
        crate::theme::install_fonts(&cc.egui_ctx);
        crate::theme::apply(&cc.egui_ctx, theme);
        let custom_languages = Vec::new();
        let keymap = KeymapOverlay::default();
        let format_on_save = false;
        let last_project = cc
            .storage
            .and_then(|s| eframe::get_value::<std::path::PathBuf>(s, LAST_PROJECT_STORAGE_KEY));
        let mut app = Self {
            active_cursor_offset: None,
            theme,
            view_mode: ViewMode::Editor,
            bottom_view: BottomView::Problems,
            project: None,
            tree: None,
            tree_scan: TreeScan::default(),
            pending_tree_scan_kind: None,
            tabs: Vec::new(),
            active_tab: None,
            untitled_count: 0,
            pending_confirm: None,
            should_quit: false,
            error: None,
            pending_create_parent: None,
            create_project_name: String::new(),
            claude: ClaudePanel::default(),
            claude_terminals: ClaudeTerminalPanel::default(),
            claude_view: ClaudeView::Chat,
            git: GitPanel::default(),
            lsp: LspBridge::default(),
            cargo: CargoPanel::default(),
            clone: CloneState::default(),
            watcher: None,
            pending_cursor_offset: None,
            hover_link: None,
            show_usages_popup: false,
            goto_action: None,
            show_goto_popup: false,
            goto_declaration_origin: None,
            pending_interface_check: None,
            show_hover_popup: false,
            last_highlighted_target: None,
            show_code_actions_popup: false,
            last_code_actions_target: None,
            show_refactor_menu_popup: false,
            show_generate_menu_popup: false,
            recent_files: Vec::new(),
            recent_files_open: false,
            recent_files_query: String::new(),
            recent_files_selected: 0,
            pending_recent_files_focus: false,
            recent_locations_open: false,
            recent_locations_selected: 0,
            via_refactor_preview: false,
            pending_refactor_preview: None,
            git_gutter: Vec::new(),
            git_gutter_path: None,
            git_gutter_popup_line: None,
            blame_popup_commit_id: None,
            pending_find_focus: false,
            search: PathSearchPanel::default(),
            search_query: String::new(),
            search_options: PathSearchOptions {
                search: ide_core::buffer_search::SearchOptions::default(),
                include: Vec::new(),
                exclude: Vec::new(),
                respect_gitignore: true,
            },
            search_include_text: String::new(),
            search_exclude_text: String::new(),
            search_replacement: String::new(),
            search_replace_open: false,
            pending_replace_in_path_preview: None,
            custom_languages,
            active_languages: Vec::new(),
            new_language_name: String::new(),
            new_language_extension: String::new(),
            new_language_command: String::new(),
            new_language_args: String::new(),
            language_settings_error: None,
            show_language_settings: false,
            dismissed_language_suggestions: Vec::new(),
            pending_language_suggestions: Vec::new(),
            command_palette_open: false,
            command_palette_query: String::new(),
            command_palette_selected: 0,
            pending_command_palette_focus: false,
            keymap,
            show_keymap_settings: false,
            keymap_search: String::new(),
            keymap_capture_target: None,
            keymap_capture_pending: None,
            keymap_import_error: None,
            nav: NavHistory::default(),
            show_project_tool_window: true,
            show_claude_tool_window: true,
            show_bottom_tool_window: true,
            zen_mode: false,
            search_everywhere_open: false,
            search_everywhere_tab: SearchEverywhereTab::Files,
            search_everywhere_query: String::new(),
            search_everywhere_selected: 0,
            pending_search_everywhere_focus: false,
            search_everywhere_class_filter: false,
            last_workspace_symbol_query: None,
            document_symbols_requested_for: None,
            file_structure_open: false,
            file_structure_query: String::new(),
            file_structure_selected: 0,
            pending_file_structure_focus: false,
            last_files_query: None,
            last_text_query: None,
            search_everywhere_text: SearchPanel::default(),
            search_everywhere_files: files_search::FilesSearchPanel::default(),
            show_go_to_line: false,
            go_to_line_input: String::new(),
            pending_go_to_line_focus: false,
            search_everywhere_double_tap: editor::double_tap::DoubleTap::default(),
            search_everywhere_shift_down: false,
            format_on_save,
            format_on_save_target: None,
            rename_popup: None,
            pending_rename_focus: false,
            pending_rename_preview: None,
        };
        app.restore_last_project(last_project, &cc.egui_ctx);
        menu::install_native_menu();
        app
    }

    // ---- pure logic (unit-tested below) ----

    fn toggle_theme(&mut self, ctx: &egui::Context) {
        self.theme = self.theme.toggled();
        crate::theme::apply(ctx, self.theme);
    }

    fn toggle_view_mode(&mut self) {
        self.view_mode = self.view_mode.toggled();
    }

    /// Whether the open project has a `Cargo.toml` at its root -- gates
    /// the Rust-specific toolbar buttons and LSP auto-start (doc §1/§3).
    fn is_rust_project(&self) -> bool {
        self.project
            .as_ref()
            .is_some_and(|p| p.root().join("Cargo.toml").exists())
    }

    /// The "Restart Language Server" action
    /// (`global-search-and-languages.md` §3, extended to every active
    /// language by `docs/features/multi-language-projects.md` §2.3):
    /// always (re)starts every entry in `active_languages`, dropping each
    /// one's previous instance first -- unlike the automatic project-
    /// open/refresh path (`sync_active_languages`), this doesn't check
    /// whether a given one is already running unchanged first. A no-op on
    /// an empty `active_languages` (no project, or nothing detected) falls
    /// straight out of `LspBridge::restart_all`'s own diff logic, no
    /// separate guard needed here.
    fn restart_lsp(&mut self) {
        if let Some(project) = &self.project {
            self.lsp.restart_all(project.root(), &self.active_languages);
        }
    }

    /// Toolbar Build/Run/Test/Check/Clippy buttons (doc §3). No-op if no
    /// project is open; `CargoPanel::run` itself handles "already running"
    /// (v1 runs at most one command at a time).
    fn run_cargo(&mut self, command: CargoCommand) {
        if let Some(project) = &self.project {
            self.cargo.run(project.root(), command);
        }
    }

    /// `fleet-shell.md` §3.3: **not** the four states `docs/roadmap.md`
    /// §4.3 mentions ("off/starting/on/error") -- `LspBridge::
    /// start_with_command` resolves synchronously (`client.is_some()` or
    /// `server_error` is set before the call returns), and nothing in
    /// `ide-lsp`'s public API exposes an "initialize handshake in flight"
    /// signal separate from "process spawned." A UI-only fake "Starting"
    /// flag that clears before the next frame paints would show nothing a
    /// user could ever observe, so it's left out rather than built as
    /// decoration.
    fn smart_mode_state(&self) -> SmartModeState {
        if self.active_languages.is_empty() {
            return SmartModeState::Off;
        }
        if self.lsp.server_error.is_some() {
            return SmartModeState::Error;
        }
        if self.lsp.is_running() {
            SmartModeState::On
        } else {
            SmartModeState::Off
        }
    }

    /// The Smart Mode indicator's click handler (§3.3): `On` stops the
    /// language server (already exists, unused from the UI until now);
    /// `Off`/`Error` (re)starts it via `restart_lsp`, unchanged behaviour.
    fn toggle_smart_mode(&mut self) {
        match self.smart_mode_state() {
            SmartModeState::On => self.lsp.stop_all(),
            SmartModeState::Off | SmartModeState::Error => self.restart_lsp(),
        }
    }

    fn is_tool_window_open(&self, window: ToolWindow) -> bool {
        match window {
            ToolWindow::Project => self.show_project_tool_window,
            ToolWindow::Claude => self.show_claude_tool_window,
            ToolWindow::Bottom => self.show_bottom_tool_window,
        }
    }

    /// `ToggleProjectToolWindow`/`ToggleClaudeToolWindow` (§3.7): a plain
    /// toggle. The three `Bottom`-targeting commands go through
    /// `toggle_bottom_tool_window` instead, which also has to pick which
    /// view becomes visible.
    fn toggle_tool_window(&mut self, window: ToolWindow) {
        match window {
            ToolWindow::Project => self.show_project_tool_window = !self.show_project_tool_window,
            ToolWindow::Claude => self.show_claude_tool_window = !self.show_claude_tool_window,
            ToolWindow::Bottom => self.show_bottom_tool_window = !self.show_bottom_tool_window,
        }
    }

    /// `ToggleFindToolWindow`/`ToggleRunToolWindow`/`ToggleProblemsToolWindow`
    /// (§3.7's exact two-branch condition): forces the Bottom tool window
    /// open on `target` if it wasn't already the visible tab, otherwise
    /// closes it -- matching JetBrains' own tool-window-shortcut behaviour.
    fn toggle_bottom_tool_window(&mut self, target: BottomView) {
        if self.show_bottom_tool_window && self.bottom_view == target {
            self.show_bottom_tool_window = false;
        } else {
            self.show_bottom_tool_window = true;
            self.bottom_view = target;
        }
    }

    /// `ToggleZenMode` (§3.8): a display-only override, doesn't reset the
    /// underlying `show_*_tool_window` flags.
    fn toggle_zen_mode(&mut self) {
        self.zen_mode = !self.zen_mode;
    }

    /// Aggregate error/warning counts across every open-project diagnostic
    /// (§3.1/§3.6's Problems indicator, shared by the top bar and the
    /// status bar so the two numbers never drift apart).
    fn problems_count(&self) -> (usize, usize) {
        let mut errors = 0;
        let mut warnings = 0;
        for diagnostics in self.lsp.diagnostics.values() {
            for diagnostic in diagnostics {
                match diagnostic.severity {
                    DiagnosticSeverity::Error => errors += 1,
                    DiagnosticSeverity::Warning => warnings += 1,
                    DiagnosticSeverity::Information | DiagnosticSeverity::Hint => {}
                }
            }
        }
        (errors, warnings)
    }

    fn open_project(&mut self, path: &Path, ctx: &egui::Context) {
        match Project::open(path) {
            Ok(project) => self.load_project(project, ctx),
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    fn create_project(&mut self, path: &Path, ctx: &egui::Context) {
        match Project::create(path) {
            Ok(project) => self.load_project(project, ctx),
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    /// Reopens the remembered `LAST_PROJECT_STORAGE_KEY` path at startup
    /// (`shell-polish-and-last-project.md` §2.2). Silent on failure --
    /// unlike `open_project`/`create_project`, this wasn't a user-
    /// initiated action this session, so a moved/deleted/invalid
    /// remembered path just falls through to the normal welcome screen
    /// instead of surfacing an error the user never asked for.
    fn restore_last_project(&mut self, path: Option<std::path::PathBuf>, ctx: &egui::Context) {
        if let Some(path) = path {
            if let Ok(project) = Project::open(&path) {
                self.load_project(project, ctx);
            }
        }
    }

    /// Clears the previous project's tree immediately (a stale,
    /// wrong-project tree must never be shown while the new one scans),
    /// then starts the scan on a background thread
    /// (`async-tree-scan.md` §3.1). Everything else here stays
    /// unchanged, synchronous, cheap disk/state work -- the freeze this
    /// doc fixes was specifically the recursive directory walk, not any
    /// of this. Also flushes the *previous* project's own settings before
    /// switching, clears `self.tabs`, and restores the new project's
    /// settings/workspace (`project-settings.md` §3.1) -- `ctx` is needed
    /// to re-apply a restored theme immediately.
    fn load_project(&mut self, project: Project, ctx: &egui::Context) {
        if let Some(old_root) = self.project.as_ref().map(|p| p.root().to_path_buf()) {
            self.flush_project_settings(&old_root);
        }
        self.tree = None;
        self.git.refresh(project.root());
        // Dropping the old watcher (if any) via plain reassignment stops
        // its background thread and OS watch before the new one starts
        // (`file-watcher.md` §3.6) -- a `WatchError::Start` degrades to
        // "tree refresh only works via the manual Refresh button" rather
        // than failing the project open.
        let watcher_error = match FileWatcher::new(project.root()) {
            Ok(watcher) => {
                self.watcher = Some(watcher);
                None
            }
            Err(e) => {
                self.watcher = None;
                Some(e.to_string())
            }
        };
        self.tree_scan.start(project.root().to_path_buf());
        self.pending_tree_scan_kind = Some(TreeScanKind::Load);
        // `self.project` must already be `Some` before `load_project_settings`
        // runs below: it restores tabs via `open_file`, which resolves each
        // tab's `.editorconfig` from `self.project` -- setting it here
        // (rather than after settings load, as `project-settings.md` §3.1
        // literally numbers the steps) is what makes that resolution see
        // the *new* project instead of `None`. `root` is captured first
        // since `project` moves into `self.project` on the next line.
        let root = project.root().to_path_buf();
        self.project = Some(project);
        self.error = watcher_error;
        self.tabs = Vec::new();
        self.active_tab = None;
        self.load_project_settings(&root, ctx);
        self.pending_create_parent = None;
        self.create_project_name.clear();
        // Discards any search or replace-preview still in flight against
        // the previous project (§3/§4) so its eventual result can't
        // overwrite `search.results`/`search.replace_preview` with the
        // wrong project's matches. Language (re)detection itself now
        // happens once the background scan completes -- see
        // `poll_tree_scan`.
        self.search.discard_in_flight();
        self.search.discard_replace_in_flight();
    }

    /// Unlike `load_project`, doesn't clear `self.tree` -- the
    /// previously scanned tree (same project) stays visible until the
    /// refreshed one arrives, avoiding a flicker to empty
    /// (`async-tree-scan.md` §3.1).
    fn refresh_tree(&mut self) {
        if let Some(project) = &self.project {
            self.git.refresh(project.root());
            self.tree_scan.start(project.root().to_path_buf());
            self.pending_tree_scan_kind = Some(TreeScanKind::Refresh);
        }
    }

    /// Drains a finished background tree scan, if any, and runs the
    /// follow-up work that depends on the new tree
    /// (`async-tree-scan.md` §3.1). Returns `true` if state changed (the
    /// caller should request a repaint), matching every other `.poll()`
    /// in this crate.
    fn poll_tree_scan(&mut self) -> bool {
        let Some(result) = self.tree_scan.poll() else {
            return false;
        };
        self.tree = result;
        match self.pending_tree_scan_kind.take() {
            Some(TreeScanKind::Load) => {
                self.resync_active_languages();
                self.refresh_language_suggestions();
            }
            Some(TreeScanKind::Refresh) => {
                // `sync_active_languages`'s own diffing already leaves an
                // unchanged, still-running language alone -- no separate
                // "is something already running" guard needed here the
                // way there used to be before
                // `docs/features/multi-language-projects.md`.
                self.resync_active_languages();
                self.refresh_language_suggestions();
            }
            None => {}
        }
        true
    }

    /// Re-runs `detect_active_languages`/`LspBridge::sync_active_languages`
    /// against the current `tree`/`custom_languages` -- shared by
    /// `load_project`, `poll_tree_scan`, `add_custom_language`, and
    /// `remove_custom_language` (`global-search-and-languages.md` §3,
    /// extended by `docs/features/multi-language-projects.md` §2.3:
    /// adding/removing a language while its project is open takes effect
    /// immediately, the same detection logic `load_project` runs on open).
    /// No-op if no project is open.
    fn resync_active_languages(&mut self) {
        let (Some(project), Some(tree)) = (&self.project, &self.tree) else {
            return;
        };
        self.active_languages = ide_core::detect_active_languages(tree, &self.custom_languages);
        self.lsp
            .sync_active_languages(project.root(), &self.active_languages);
    }

    /// Recomputes `pending_language_suggestions` from the current project
    /// root, `custom_languages`, and `dismissed_language_suggestions`
    /// (`docs/features/language-auto-detect.md` §3.2). No-op (clears the
    /// list) if no project is open. Unlike before `docs/features/
    /// multi-language-projects.md`, Rust being active no longer suppresses
    /// every other suggestion -- several languages can be active at once
    /// now, so a `go.mod` suggestion in a Rust-rooted project can
    /// genuinely activate.
    fn refresh_language_suggestions(&mut self) {
        let Some(project) = &self.project else {
            self.pending_language_suggestions.clear();
            return;
        };
        let mut suggestions = ide_core::detect_language_suggestions(project.root());
        suggestions.retain(|s| {
            !self
                .dismissed_language_suggestions
                .iter()
                .any(|d| d == &s.marker_file)
                && !self
                    .custom_languages
                    .iter()
                    .any(|c| c.extension.eq_ignore_ascii_case(&s.config.extension))
        });
        self.pending_language_suggestions = suggestions;
    }

    /// "Enable" on the language-suggestion popup (`docs/features/
    /// language-auto-detect.md` §2.2/§3.4): pushes `suggestion.config`
    /// into `custom_languages` (identical effect to typing it into
    /// "Languages…" and clicking Add -- a marker-sourced config can never
    /// collide with an existing extension, since `refresh_language_
    /// suggestions` already filtered that case out, so this skips
    /// `add_custom_language`'s own validation), removes `suggestion` from
    /// `pending_language_suggestions`, and re-runs `resync_active_languages` so
    /// the new config's LSP starts immediately if it's now the detected
    /// language.
    fn enable_language_suggestion(&mut self, suggestion: ide_core::LanguageSuggestion) {
        self.custom_languages.push(suggestion.config);
        self.pending_language_suggestions
            .retain(|s| s.marker_file != suggestion.marker_file);
        self.resync_active_languages();
    }

    /// "Dismiss" on the language-suggestion popup, and its close button
    /// (`docs/features/language-auto-detect.md` §2.2/§3.4): records
    /// `suggestion.marker_file` as declined for this project (skipped if
    /// already present, so resolving the same marker twice doesn't
    /// duplicate the entry) and removes `suggestion` from
    /// `pending_language_suggestions`. Never touches `custom_languages`.
    fn dismiss_language_suggestion(&mut self, suggestion: ide_core::LanguageSuggestion) {
        if !self
            .dismissed_language_suggestions
            .iter()
            .any(|d| d == &suggestion.marker_file)
        {
            self.dismissed_language_suggestions
                .push(suggestion.marker_file.clone());
        }
        self.pending_language_suggestions
            .retain(|s| s.marker_file != suggestion.marker_file);
    }

    /// Best-effort canonicalization for a path about to enter tab state
    /// (`open_file`, `save_active_as`) -- `file-watcher.md` §3.4's "Path
    /// identity" invariant: every `Tab::buffer.path()` must be canonical so
    /// `poll_watcher`'s `WatchEvent` paths (always canonical) can match it
    /// with a plain equality instead of a re-canonicalization on every
    /// poll. Falls back to the parent directory's canonical form joined
    /// with the file name when `path` doesn't exist yet (a brand-new file
    /// via Save As) -- the same technique `ide_core::FileWatcher` uses
    /// internally for a path that doesn't (yet, or any longer) exist, so
    /// the result matches whatever `FileWatcher`'s own fold-time
    /// canonicalization will eventually produce for the same file. Falls
    /// back to the raw path unchanged if even the parent can't be resolved
    /// -- lets the caller's normal error path (`Buffer::open`/`save_as`
    /// failing) surface instead of this silently swallowing the problem.
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

    /// Builds `ProjectPreferences`/`WorkspaceState` from current in-memory
    /// state and writes both to `root` (`project.root()`; `project-
    /// settings.md` §2.2 specifies `&Project` here, but this only ever
    /// needs `root()` and taking `&Path` instead avoids an aliasing
    /// conflict at the one call site inside `load_project` that matters --
    /// see its own doc comment). Called from `save()` and from
    /// `load_project` right before switching away from the currently-open
    /// project (§3.1). A write failure is swallowed (matches
    /// `eframe::App::save`'s `()` return -- there is no caller to
    /// propagate an error to); this frame's settings simply don't persist,
    /// and the next successful write catches up.
    fn flush_project_settings(&mut self, root: &Path) {
        let preferences = ProjectPreferences {
            theme: self.theme,
            custom_languages: self.custom_languages.clone(),
            keymap: self.keymap.clone(),
            format_on_save: self.format_on_save,
            dismissed_language_suggestions: self.dismissed_language_suggestions.clone(),
        };
        let _ = project_settings::write(root, ProjectSettingsFile::Preferences, &preferences);

        let open_tabs: Vec<OpenTabState> = self
            .tabs
            .iter()
            .filter_map(|tab| {
                let path = tab.buffer.path()?;
                let path = path.strip_prefix(root).ok()?.to_path_buf();
                let cursor_offset = tab.buffer.text_buffer().selections().primary().head;
                Some(OpenTabState {
                    path,
                    cursor_offset,
                })
            })
            .collect();
        let active_path = self
            .active_tab
            .and_then(|idx| self.tabs.get(idx))
            .and_then(|tab| tab.buffer.path())
            .and_then(|path| path.strip_prefix(root).ok())
            .map(Path::to_path_buf);
        let recent_files: Vec<PathBuf> = self
            .recent_files
            .iter()
            .filter_map(|path| path.strip_prefix(root).ok())
            .map(Path::to_path_buf)
            .collect();
        let workspace = WorkspaceState {
            open_tabs,
            active_path,
            recent_files,
        };
        let _ = project_settings::write(root, ProjectSettingsFile::Workspace, &workspace);
    }

    /// Reads `root`'s `.ide/preferences.json` (or
    /// `ProjectPreferences::default()` if absent/malformed) into `self`,
    /// re-applying the theme to `ctx` immediately. Then reads
    /// `.ide/workspace.json` and restores up to `MAX_RESTORED_TABS` tabs
    /// (§3.3) -- capped since `workspace.json` is untrusted (see
    /// `MAX_RESTORED_TABS`'s doc comment). Takes `root: &Path` rather than
    /// `&Project` for the same reason as `flush_project_settings` -- see
    /// its doc comment.
    fn load_project_settings(&mut self, root: &Path, ctx: &egui::Context) {
        let preferences =
            project_settings::read::<ProjectPreferences>(root, ProjectSettingsFile::Preferences)
                .ok()
                .flatten()
                .unwrap_or_default();
        self.theme = preferences.theme;
        crate::theme::apply(ctx, self.theme);
        self.custom_languages = preferences.custom_languages;
        self.keymap = preferences.keymap;
        self.format_on_save = preferences.format_on_save;
        self.dismissed_language_suggestions = preferences.dismissed_language_suggestions;

        let workspace =
            project_settings::read::<WorkspaceState>(root, ProjectSettingsFile::Workspace)
                .ok()
                .flatten()
                .unwrap_or_default();
        self.recent_files = workspace
            .recent_files
            .iter()
            .filter_map(|relative| Self::resolve_restorable_tab_path(root, relative))
            .collect();

        for state in workspace.open_tabs.iter().take(MAX_RESTORED_TABS) {
            let Some(canonical) = Self::resolve_restorable_tab_path(root, &state.path) else {
                continue;
            };
            self.open_file(&canonical);
            let Some(idx) = self.active_tab else {
                continue;
            };
            if self.tabs[idx].buffer.path() != Some(canonical.as_path()) {
                continue;
            }
            let len = self.tabs[idx].buffer.text().len();
            let mut offset = state.cursor_offset.min(len);
            {
                let text = self.tabs[idx].buffer.text();
                while offset > 0 && !text.is_char_boundary(offset) {
                    offset -= 1;
                }
            }
            self.tabs[idx]
                .buffer
                .text_buffer_mut()
                .set_selections(Selections::single(Selection::caret(offset)));
        }

        self.active_tab = workspace
            .active_path
            .as_ref()
            .and_then(|rel| {
                let canonical = Self::resolve_restorable_tab_path(root, rel)?;
                self.tabs
                    .iter()
                    .position(|t| t.buffer.path() == Some(canonical.as_path()))
            })
            .or(if self.tabs.is_empty() { None } else { Some(0) });
    }

    /// The path-safety check for a workspace-restore path (doc §2.1's
    /// "Path safety" note / §3.3): rejects `relative` outright if it's
    /// absolute or contains a `..` component (`Path::join` would otherwise
    /// silently discard `root` for an absolute `relative`), then
    /// canonicalizes the joined result and requires it to both stay
    /// inside `root` and still exist on disk. `None` on any failure -- the
    /// caller skips that tab exactly like a missing file (§3.3), never
    /// treats this as an error to surface.
    fn resolve_restorable_tab_path(root: &Path, relative: &Path) -> Option<PathBuf> {
        let safe = relative.components().all(|c| {
            matches!(
                c,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        });
        if !safe {
            return None;
        }
        let canonical = Self::canonicalize_best_effort(&root.join(relative));
        if !canonical.starts_with(root) || !canonical.exists() {
            return None;
        }
        Some(canonical)
    }

    /// Opens `path` in a tab, focusing an already-open tab for the same
    /// path instead of duplicating it. `path` must come from `scan_tree()`,
    /// an explicit native-dialog result, or a validated workspace-restore
    /// path (`resolve_restorable_tab_path`, `project-settings.md` §2.1) —
    /// never from Claude panel text (doc §4's path-provenance invariant;
    /// enforced by callers only ever passing paths from those sources, see
    /// `App::ui`).
    /// Moves `path` to the front of `recent_files` if already present,
    /// inserts at the front otherwise, truncates to `MAX_RECENT_FILES`
    /// (`docs/features/recent-files.md` §2.4). Called from both of
    /// `open_file`'s branches -- every successful open or refocus counts
    /// as "recently used". In-memory only -- does not itself write
    /// `workspace.json` (that only happens from `flush_project_settings`,
    /// matching every other workspace-state field's persistence cadence).
    fn record_recent_file(&mut self, path: PathBuf) {
        self.recent_files.retain(|p| *p != path);
        self.recent_files.insert(0, path);
        self.recent_files.truncate(MAX_RECENT_FILES);
    }

    fn open_file(&mut self, path: &Path) {
        let canonical = Self::canonicalize_best_effort(path);
        let path = canonical.as_path();
        if let Some(idx) = self.tabs.iter().position(|t| t.buffer.path() == Some(path)) {
            self.active_tab = Some(idx);
            self.record_recent_file(path.to_path_buf());
            return;
        }
        match Buffer::open(path) {
            Ok(buffer) => {
                let text = buffer.text().to_string();
                let config = self.resolve_editor_config(path);
                self.tabs.push(Tab::from_buffer(buffer));
                let idx = self.tabs.len() - 1;
                self.tabs[idx].apply_editor_config(config);
                self.active_tab = Some(idx);
                self.error = None;
                self.record_recent_file(path.to_path_buf());
                self.lsp.send(
                    path,
                    LspRequest::DidOpen {
                        path: path.to_path_buf(),
                        text,
                    },
                );
                self.sync_inlay_hints(idx);
                self.sync_semantic_tokens(idx);
                self.sync_document_symbols(idx);
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    /// `EditorConfig::default()` (every property `None`, so an untitled
    /// buffer or a file outside any open project falls back to
    /// `IndentUnit::default()`) for anything `editorconfig::resolve` can't
    /// give a real answer for -- no project open, or `path` resolves
    /// outside its root. `line-commands-and-editorconfig.md` §3.6 treats
    /// `OutsideRoot` the same way: as "no config", never as an error the
    /// caller must surface.
    fn resolve_editor_config(&self, path: &Path) -> EditorConfig {
        self.project
            .as_ref()
            .and_then(|project| editorconfig::resolve(project.root(), path).ok())
            .unwrap_or_default()
    }

    /// Opens `path` and best-effort arranges for the cursor to land at
    /// `position` once the tab renders -- shared navigation logic behind
    /// both `open_diagnostic` (Problems panel row click) and `open_usage`
    /// (Usages panel row click, doc §2.2).
    fn open_at(&mut self, path: &Path, position: Position) {
        self.open_file(path);
        let Some(idx) = self.active_tab else { return };
        self.pending_cursor_offset =
            ide_lsp::position_to_byte_offset(self.tabs[idx].buffer.text(), position);
        self.push_nav_location();
    }

    /// Opens `path` (a diagnostic's file, from the Problems panel -- must
    /// come from `LspBridge::diagnostics`' keys, the same path-provenance
    /// discipline as `open_file`'s own doc comment) and best-effort
    /// arranges for the cursor to land at `start` once the tab renders.
    fn open_diagnostic(&mut self, path: &Path, start: Position) {
        self.open_at(path, start);
    }

    /// Opens `path` (a find-usages result's file -- must come from
    /// `LspBridge::references` entries, the same path-provenance discipline
    /// as `open_file`/`open_diagnostic`) and best-effort arranges for the
    /// cursor to land at `position` once the tab renders.
    fn open_usage(&mut self, path: &Path, position: Position) {
        self.open_at(path, position);
    }

    /// Opens `path` (a `Goto` result's file -- must come from
    /// `LspBridge::goto` entries, already validated against `project_root`
    /// inside `ide-lsp`, the same path-provenance discipline as
    /// `open_usage`) and best-effort arranges for the cursor to land at
    /// `position` once the tab renders. Shared by the jump-immediately path
    /// (`handle_goto_response`, exactly one result) and a Go to popup row
    /// click (`docs/features/goto-definition.md` §2.2/§3.3/§3.4).
    fn open_definition(&mut self, path: &Path, position: Position) {
        self.open_at(path, position);
    }

    /// Opens `path` (a search result's file -- must come from a
    /// `SearchMatch` the running search itself produced, the same
    /// path-provenance discipline as `open_file`/`open_diagnostic`/
    /// `open_usage`, `global-search-and-languages.md` §4) and best-effort
    /// places the cursor at `byte_offset`. A `SearchMatch` already carries
    /// an absolute byte offset, so unlike `open_at` there's no
    /// `Position`-conversion step -- kept as its own small sibling rather
    /// than folded into `open_at`, matching this project's "three similar
    /// lines is better than a premature abstraction" preference.
    fn open_search_result(&mut self, path: &Path, byte_offset: usize) {
        self.open_file(path);
        self.pending_cursor_offset = Some(byte_offset);
        self.push_nav_location();
    }

    /// Pushes the active tab's path and the offset it's about to jump to
    /// (`pending_cursor_offset`, or `0` when nothing is pending -- the
    /// "wherever the cursor lands" case for a plain `open_file`, doc §3.2's
    /// example) onto `nav`. No-op with no active tab or an untitled one
    /// (no path to return to). Called from every jump site that already
    /// moves the cursor to an arbitrary location: the tree click handler
    /// (after `open_file`), `open_at` (shared by `open_diagnostic`/
    /// `open_usage`), and `open_search_result` above -- **never** from
    /// `nav_back`/`nav_forward` themselves, or every Back press would
    /// immediately push a new forward-erasing entry (§3.2/§4's invariant).
    fn push_nav_location(&mut self) {
        let Some(idx) = self.active_tab else { return };
        let Some(path) = self.tabs[idx].buffer.path().map(Path::to_path_buf) else {
            return;
        };
        let offset = self.pending_cursor_offset.unwrap_or(0);
        self.nav.push(NavLocation { path, offset });
    }

    /// Top bar's back arrow (§3.1/§3.2): opens the previous location's file
    /// (if not already active) and arranges for the cursor to land at its
    /// offset, reusing the same `open_file` + `pending_cursor_offset`
    /// mechanism every other jump source uses -- deliberately **not**
    /// calling `push_nav_location` (§3.2/§4's invariant). No-op at the
    /// oldest entry.
    fn nav_back(&mut self) {
        let Some(location) = self.nav.go_back() else {
            return;
        };
        self.open_file(&location.path);
        self.pending_cursor_offset = Some(location.offset);
    }

    /// Top bar's forward arrow. No-op at the newest entry.
    fn nav_forward(&mut self) {
        let Some(location) = self.nav.go_forward() else {
            return;
        };
        self.open_file(&location.path);
        self.pending_cursor_offset = Some(location.offset);
    }

    /// `⌘E`'s entry point (`docs/features/recent-files.md` §2.4). An empty
    /// list isn't an error state -- the popup opens either way and shows
    /// its own empty-state message (§3.2). Closes Recent Locations if
    /// open (§2.4/§4's two-popup-only exclusivity).
    fn trigger_recent_files(&mut self) {
        self.recent_files_open = true;
        self.recent_locations_open = false;
        self.recent_files_query.clear();
        self.recent_files_selected = 0;
        self.pending_recent_files_focus = true;
    }

    fn close_recent_files(&mut self) {
        self.recent_files_open = false;
    }

    /// Checked in `handle_shortcuts`'s escape-arbitration chain alongside
    /// `file_structure_owns_escape` -- a small standalone modal, not part
    /// of the palette/Search Everywhere's own mutual-exclusion group.
    fn recent_files_owns_escape(&self) -> bool {
        self.recent_files_open
    }

    /// Empty query: `recent_files` verbatim (MRU order). Non-empty query:
    /// fuzzy-scored against each entry's *project-relative* display path,
    /// not the absolute canonical path -- scoring the absolute path would
    /// let an unrelated segment of a canonicalized temp/home directory
    /// spuriously match every entry (§2.4). Dropped on `None`, sorted by
    /// score descending.
    fn recent_files_rows(&self) -> Vec<PathBuf> {
        if self.recent_files_query.is_empty() {
            return self.recent_files.clone();
        }
        let root = self.project.as_ref().map(|p| p.root());
        let mut scored: Vec<(i64, PathBuf)> = self
            .recent_files
            .iter()
            .filter_map(|path| {
                let display = match root {
                    Some(root) => path
                        .strip_prefix(root)
                        .unwrap_or(path)
                        .display()
                        .to_string(),
                    None => path.display().to_string(),
                };
                let m = ide_core::fuzzy_score(&self.recent_files_query, &display)?;
                Some((m.score, path.clone()))
            })
            .collect();
        scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        scored.into_iter().map(|(_, path)| path).collect()
    }

    /// `↑`/`↓` while the popup is open (§2.4), clamped -- not wrapping
    /// like `search_everywhere_move_selection`/`file_structure_move_
    /// selection`, since Recent Files' MRU-ordered list has a meaningful
    /// "top", unlike those two.
    fn recent_files_move_selection(&mut self, delta: isize) {
        let count = self.recent_files_rows().len();
        if count == 0 {
            return;
        }
        let next = (self.recent_files_selected as isize + delta).clamp(0, count as isize - 1);
        self.recent_files_selected = next as usize;
    }

    /// `Enter`, or clicking a row (§2.4): opens the selected row via
    /// `open_file` -- deliberately does not set `pending_cursor_offset`,
    /// preserving whatever cursor position that tab already has (Recent
    /// Files' point is "go back to where I already was", unlike Recent
    /// Locations' jump-to-offset semantics). No-op if the selection is out
    /// of range for the current (possibly filtered) row list.
    fn recent_files_confirm(&mut self) {
        let rows = self.recent_files_rows();
        let Some(path) = rows.get(self.recent_files_selected).cloned() else {
            return;
        };
        self.close_recent_files();
        self.open_file(&path);
    }

    /// `⌘⇧E`'s entry point. Same open-regardless-of-emptiness posture as
    /// `trigger_recent_files`; closes Recent Files if open.
    fn trigger_recent_locations(&mut self) {
        self.recent_locations_open = true;
        self.recent_files_open = false;
        self.recent_locations_selected = 0;
    }

    fn close_recent_locations(&mut self) {
        self.recent_locations_open = false;
    }

    fn recent_locations_owns_escape(&self) -> bool {
        self.recent_locations_open
    }

    /// `self.nav.recent_locations()`, taken up to
    /// `MAX_RECENT_LOCATIONS_SHOWN`, paired with a 1-based display line
    /// (`ide_lsp::byte_offset_to_position`'s 0-based `Position.line`,
    /// `+1` for display) and a one-line preview: the trimmed text of the
    /// line containing `offset`, read from the open tab's live buffer if
    /// that path is currently open, else a best-effort disk read
    /// (`docs/features/recent-files.md` §2.4) -- the same read-only,
    /// no-extra-validation-needed shape `apply_workspace_edit`'s existing
    /// fallback already uses, since these paths only ever originate from
    /// `self.nav`, itself only ever pushed from already-validated jump
    /// sites (`push_nav_location`'s own doc comment). Both `None` if the
    /// read fails or `offset` no longer lands inside the file's current
    /// length.
    fn recent_locations_rows(&self) -> Vec<(NavLocation, Option<u32>, Option<String>)> {
        self.nav
            .recent_locations()
            .take(MAX_RECENT_LOCATIONS_SHOWN)
            .map(|location| {
                let text = self
                    .tabs
                    .iter()
                    .find(|tab| tab.buffer.path() == Some(location.path.as_path()))
                    .map(|tab| tab.buffer.text().to_string())
                    .or_else(|| std::fs::read_to_string(&location.path).ok());
                let (line, preview) = match text {
                    Some(text) => match ide_lsp::byte_offset_to_position(&text, location.offset) {
                        Some(position) => {
                            let line_start =
                                text[..location.offset].rfind('\n').map_or(0, |i| i + 1);
                            let line_end = text[location.offset..]
                                .find('\n')
                                .map(|i| location.offset + i)
                                .unwrap_or(text.len());
                            (
                                Some(position.line + 1),
                                Some(text[line_start..line_end].trim().to_string()),
                            )
                        }
                        None => (None, None),
                    },
                    None => (None, None),
                };
                (location.clone(), line, preview)
            })
            .collect()
    }

    /// `↑`/`↓` while the popup is open, clamped -- same rationale as
    /// `recent_files_move_selection`.
    fn recent_locations_move_selection(&mut self, delta: isize) {
        let count = self.recent_locations_rows().len();
        if count == 0 {
            return;
        }
        let next = (self.recent_locations_selected as isize + delta).clamp(0, count as isize - 1);
        self.recent_locations_selected = next as usize;
    }

    /// `Enter`, or clicking a row: opens the location's file and sets
    /// `pending_cursor_offset` to its `offset` -- the exact `nav_back`/
    /// `nav_forward` mechanism, deliberately not calling
    /// `push_nav_location` (jumping *from* Recent Locations shouldn't
    /// itself grow the history it's displaying). No-op if the selection
    /// is out of range.
    fn recent_locations_confirm(&mut self) {
        let rows = self.recent_locations_rows();
        let Some((location, _, _)) = rows.into_iter().nth(self.recent_locations_selected) else {
            return;
        };
        self.close_recent_locations();
        self.open_file(&location.path);
        self.pending_cursor_offset = Some(location.offset);
    }

    /// Computes what `find_usages` would query, without touching `self.lsp`
    /// -- kept as its own method (rather than inlined into `find_usages`)
    /// so its no-op conditions are independently unit-testable without
    /// requiring a running `LspClient` in the test harness (`LspBridge::
    /// find_references` itself already no-ops with no client running,
    /// which would otherwise mask every one of these conditions from a
    /// test observing only `finding_references`/`references`). `None` if
    /// there's no active tab, the tab has no path (an unsaved buffer), the
    /// cursor offset isn't known yet, the offset doesn't land on a valid
    /// position, or `view_mode != ViewMode::Editor` -- the last check
    /// matters because `active_cursor_offset` isn't cleared on a view
    /// switch, so without it `Alt+F7` fired while Source Control is
    /// showing would fire on a stale offset left over from the last editor
    /// frame (doc §2.2). Matches doc §4's provenance rule that this call's
    /// inputs only ever come from the active tab's own path and text.
    fn find_usages_target(&self) -> Option<(PathBuf, Position)> {
        if self.view_mode != ViewMode::Editor {
            return None;
        }
        let idx = self.active_tab?;
        let path = self.tabs[idx].buffer.path()?.to_path_buf();
        let offset = self.active_cursor_offset?;
        let position = ide_lsp::byte_offset_to_position(self.tabs[idx].buffer.text(), offset)?;
        Some((path, position))
    }

    /// Alt+F7 / toolbar "Find Usages" / Cmd+Click entry point (doc §3): see
    /// `find_usages_target` for the no-op conditions.
    fn find_usages(&mut self) {
        if let Some((path, position)) = self.find_usages_target() {
            self.lsp.find_references(&path, position);
        }
    }

    /// `find_usages` plus switching the bottom panel to the Usages view --
    /// shared by the toolbar button and Alt+F7 (find-usages doc §3). The
    /// navigation gestures use `trigger_find_usages_popup` instead.
    fn trigger_find_usages(&mut self) {
        self.find_usages();
        self.bottom_view = BottomView::Usages;
    }

    /// `find_usages` plus raising the floating Usages window -- the
    /// `Cmd+B` / `Cmd+Click` entry point
    /// (`richer-highlighting-and-usages-popup.md` §3). The window opens
    /// even when the query itself no-ops (no active tab, no language
    /// server): it renders "No usages found." rather than swallowing the
    /// gesture silently.
    fn trigger_find_usages_popup(&mut self) {
        self.find_usages();
        self.show_usages_popup = true;
    }

    /// `Cmd+B` / `Cmd+Click` entry point (`docs/features/
    /// goto-definition.md` §3.1): shares `find_usages_target`'s no-op
    /// gating unchanged (reused as-is despite its find-usages-specific
    /// name -- §4). Sets `goto_action` and force-clears `show_goto_popup`
    /// before sending, so a second gesture fired while a popup from a
    /// previous one is still open closes it immediately rather than
    /// leaving stale rows visible under a new query (§4).
    fn trigger_go_to_declaration(&mut self) {
        self.goto_action = Some(GotoKind::Definition);
        self.show_goto_popup = false;
        self.pending_interface_check = None;
        if let Some((path, position)) = self.find_usages_target() {
            self.goto_declaration_origin = Some((path.clone(), position));
            self.lsp.go_to_definition(&path, position);
        } else {
            self.goto_declaration_origin = None;
        }
    }

    /// `Ctrl+Shift+B` entry point. Same shape as `trigger_go_to_declaration`.
    fn trigger_go_to_type_declaration(&mut self) {
        self.goto_action = Some(GotoKind::TypeDefinition);
        self.show_goto_popup = false;
        if let Some((path, position)) = self.find_usages_target() {
            self.lsp.go_to_type_definition(&path, position);
        }
    }

    /// `Cmd+Option+B` entry point. Same shape as `trigger_go_to_declaration`.
    fn trigger_go_to_implementation(&mut self) {
        self.goto_action = Some(GotoKind::Implementation);
        self.show_goto_popup = false;
        if let Some((path, position)) = self.find_usages_target() {
            self.lsp.go_to_implementation(&path, position);
        }
    }

    /// Called once per frame, immediately after `self.lsp.poll()`
    /// (`docs/features/goto-definition.md` §3.3). No-op unless
    /// `self.lsp.goto_ready` -- exactly one result jumps immediately with
    /// no popup ever shown (the common case); zero or more than one opens
    /// `show_goto_popup` instead (rendered by `render_goto_popup`), even
    /// for zero, so the gesture always visibly acknowledges itself.
    ///
    /// For a `Definition` query's single-result case, the jump is deferred
    /// to `handle_interface_check_response` instead of happening here
    /// directly -- see `docs/features/goto-declaration-interface-
    /// redirect.md` §2.2. Every other case (zero/many results, or any
    /// non-`Definition` `goto_action`) is unaffected.
    fn handle_goto_response(&mut self) {
        if !self.lsp.goto_ready {
            return;
        }
        match self.lsp.goto.len() {
            1 if self.goto_action == Some(GotoKind::Definition) => {
                let location = self.lsp.goto[0].clone();
                self.pending_interface_check = Some(location.clone());
                self.lsp.request_document_symbols(&location.path);
            }
            1 => {
                let path = self.lsp.goto[0].path.clone();
                let position = self.lsp.goto[0].range.start;
                self.open_definition(&path, position);
            }
            _ => {
                self.show_goto_popup = true;
            }
        }
    }

    /// Called once per frame, immediately after `handle_goto_response`
    /// (`docs/features/goto-declaration-interface-redirect.md` §2.2).
    /// No-op unless `self.lsp.document_symbols_ready` and
    /// `pending_interface_check` is `Some` and the fresh response's path
    /// matches the pending location's -- a `DocumentSymbol` answer for
    /// some other file (e.g. a stale in-flight File Structure query) is
    /// left untouched for its own intended consumer, and this check's own
    /// request stays outstanding until its own matching response arrives.
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
        if position_is_within_interface(&self.lsp.document_symbols, location.range.start) {
            match self.goto_declaration_origin.clone() {
                Some((origin_path, origin_position)) => {
                    self.goto_action = Some(GotoKind::Implementation);
                    self.lsp.go_to_implementation(&origin_path, origin_position);
                }
                None => {
                    self.open_definition(&location.path, location.range.start);
                }
            }
        } else {
            self.open_definition(&location.path, location.range.start);
        }
    }

    /// `F1` (`Ctrl+Q` off macOS) entry point (`docs/features/
    /// inlay-hints-and-hover.md` §3.1): shares `find_usages_target`'s
    /// no-op gating unchanged (reused as-is despite its find-usages-
    /// specific name, a fourth caller -- §4). Unlike the Goto popup, opens
    /// immediately and shows a loading state while `self.lsp.finding_hover`
    /// is true -- there is always exactly one hover answer to show, never
    /// zero-or-many, so there's no jump-vs-popup branch to defer opening
    /// for (§3.1).
    fn trigger_quick_documentation(&mut self) {
        self.show_hover_popup = true;
        if let Some((path, position)) = self.find_usages_target() {
            self.lsp.request_hover(&path, position);
        }
    }

    /// Called once per frame, immediately after `self.lsp.poll()` (§3.4).
    /// Fires a fresh `DocumentHighlight` query whenever `find_usages_target`
    /// names a different `(path, position)` than the last one queried;
    /// clears `document_highlights` (without sending anything) when there's
    /// no valid target at all, so switching to a tab with no target doesn't
    /// leave the previous file's highlights bleeding through.
    fn sync_document_highlights(&mut self) {
        match self.find_usages_target() {
            Some(target) if Some(target.clone()) != self.last_highlighted_target => {
                self.lsp.request_document_highlight(&target.0, target.1);
                self.last_highlighted_target = Some(target);
            }
            Some(_) => {}
            None => {
                self.lsp.clear_document_highlights();
                self.last_highlighted_target = None;
            }
        }
    }

    /// Called once per frame, immediately after `self.lsp.poll()`, alongside
    /// `sync_document_highlights` (`docs/features/code-actions.md` §2.3).
    /// Same `find_usages_target`-driven shape: a new target fires
    /// `LspBridge::request_code_actions` and updates
    /// `last_code_actions_target`; no target clears `lsp.code_actions` and
    /// `last_code_actions_target` via `LspBridge::clear_code_actions`.
    fn sync_code_actions(&mut self) {
        match self.find_usages_target() {
            Some(target) if Some(target.clone()) != self.last_code_actions_target => {
                self.lsp.request_code_actions(&target.0, target.1);
                self.last_code_actions_target = Some(target);
            }
            Some(_) => {}
            None => {
                self.lsp.clear_code_actions();
                self.last_code_actions_target = None;
            }
        }
    }

    /// `⌥↩` entry point (`docs/features/code-actions.md` §2.3). Unlike
    /// `trigger_quick_documentation`, this does **not** send a new request:
    /// `sync_code_actions` already keeps `lsp.code_actions` reasonably fresh
    /// ambiently, so this just opens the popup on whatever's already
    /// cached (with the same "up to one round trip" latency `document_
    /// highlights` already has if the caret only just moved this frame).
    fn trigger_show_intention_actions(&mut self) {
        self.show_code_actions_popup = true;
    }

    /// A code actions popup row click: closes the popup, sends
    /// `LspRequest::ApplyCodeAction { index }` (§3.3).
    fn select_code_action(&mut self, index: usize) {
        self.show_code_actions_popup = false;
        self.lsp.apply_code_action(index);
    }

    /// `⌃T`'s entry point (`docs/features/refactor-this.md` §3.1). No new
    /// request -- same "open on whatever's cached" shape `trigger_show_
    /// intention_actions` already established. No-op with an error if no
    /// cached action's `kind` starts with `"refactor"`.
    fn trigger_refactor_this(&mut self) {
        if !self
            .lsp
            .code_actions
            .iter()
            .any(|a| a.kind.as_deref().is_some_and(|k| k.starts_with("refactor")))
        {
            self.error = Some("Refactor This: no refactoring available here".to_string());
            return;
        }
        self.show_refactor_menu_popup = true;
    }

    /// A `⌃T` popup row click.
    fn select_refactor_action(&mut self, index: usize) {
        self.show_refactor_menu_popup = false;
        self.apply_code_action_via_preview(index);
    }

    /// The five direct commands' shared entry point (§3.2). No-op with an
    /// error if no cached, non-disabled action matches `kind`'s heuristic.
    fn trigger_direct_refactor(&mut self, kind: DirectRefactorKind) {
        let found = self
            .lsp
            .code_actions
            .iter()
            .find(|a| a.disabled_reason.is_none() && kind.matches(a))
            .map(|a| a.index);
        match found {
            Some(index) => self.apply_code_action_via_preview(index),
            None => self.error = Some(format!("{}: not available here", kind.name())),
        }
    }

    /// `⌘N`/`Alt+Insert`'s entry point (`docs/features/code-generation.md`
    /// §3.1). Same "open on whatever's cached" shape `trigger_refactor_
    /// this` established, filtered to `kind.as_deref() == Some("")`
    /// instead of a `starts_with` prefix (§1) -- no-op with an error if no
    /// cached action matches.
    fn trigger_generate_menu(&mut self) {
        if !self
            .lsp
            .code_actions
            .iter()
            .any(|a| a.kind.as_deref() == Some(""))
        {
            self.error = Some("Generate: nothing to generate here".to_string());
            return;
        }
        self.show_generate_menu_popup = true;
    }

    /// A Generate popup row click: closes the popup, applies immediately
    /// -- not the Refactor Preview path D2's popup uses (§2.2, §3.1).
    /// `select_code_action` only clears `show_code_actions_popup` (a
    /// different flag), so `show_generate_menu_popup` is cleared here
    /// explicitly rather than by reuse alone.
    fn select_generate_action(&mut self, index: usize) {
        self.show_generate_menu_popup = false;
        self.select_code_action(index)
    }

    /// `Ctrl+I`/`Ctrl+O`/`⌘⇧T`'s shared entry point (§2.2, §3.2/§3.3) --
    /// same shape as `trigger_direct_refactor`, but applies immediately
    /// rather than going through `apply_code_action_via_preview`.
    fn trigger_direct_generate(&mut self, kind: DirectGenerateKind) {
        let found = self
            .lsp
            .code_actions
            .iter()
            .find(|a| a.disabled_reason.is_none() && kind.matches(a))
            .map(|a| a.index);
        match found {
            Some(index) => self.lsp.apply_code_action(index),
            None => self.error = Some(format!("{}: not available here", kind.name())),
        }
    }

    /// Optimize Imports' entry point (palette-only, §2.2, §3.4). Unlike
    /// every other command in this phase, reaches for nothing in
    /// `lsp.code_actions` -- sends a fresh `LspRequest::OrganizeImports`
    /// every time it's invoked, since the ambient per-caret cache is the
    /// wrong data source for a whole-file, `context.only`-scoped request.
    fn trigger_optimize_imports(&mut self) {
        // Reuses `find_usages_target` purely for its path half -- the same
        // "no active tab / no path" `None` case every other per-file
        // command already treats as a silent no-op.
        let Some((path, _)) = self.find_usages_target() else {
            return;
        };
        self.lsp.request_organize_imports(&path);
    }

    /// Shared by `select_refactor_action` and `trigger_direct_refactor`:
    /// routes the resulting `WorkspaceEditReady` into the Refactor
    /// Preview dialog instead of `handle_workspace_edit_ready`'s ordinary
    /// immediate-apply path (§3.4).
    fn apply_code_action_via_preview(&mut self, index: usize) {
        self.via_refactor_preview = true;
        self.lsp.apply_code_action(index);
    }

    /// Builds `pending_refactor_preview` from a ready `WorkspaceEdit`
    /// (§3.3): for each file, reads old text (open tab's buffer if any,
    /// else a fresh disk read -- same source `apply_workspace_edit`
    /// already reads from), computes new text via `workspace_text_edits_
    /// to_transaction` + `ide_core::apply_transaction`, and diffs old/new
    /// via `ide_core::diff_text`. Read-only -- never touches disk or any
    /// buffer.
    fn show_refactor_preview(&mut self, what: String, edit: ide_lsp::WorkspaceEdit) {
        let diffs = edit
            .edits
            .iter()
            .map(|file_edit| {
                let open_tab = self
                    .tabs
                    .iter()
                    .position(|tab| tab.buffer.path() == Some(file_edit.path.as_path()));
                let old_text = match open_tab {
                    Some(idx) => self.tabs[idx].buffer.text().to_string(),
                    None => std::fs::read_to_string(&file_edit.path).ok()?,
                };
                let transaction =
                    workspace_text_edits_to_transaction(&old_text, &file_edit.text_edits)?;
                let new_text = ide_core::apply_transaction(&old_text, &transaction)?;
                ide_core::diff_text(&file_edit.path, &old_text, &new_text)
            })
            .collect();
        self.pending_refactor_preview = Some(RefactorPreview { what, edit, diffs });
    }

    /// The preview's Apply button (§3.5): reuses the existing shared
    /// `apply_workspace_edit` primitive, same success/failure message
    /// shape every other apply path uses.
    fn confirm_refactor_preview(&mut self) {
        let Some(preview) = self.pending_refactor_preview.take() else {
            return;
        };
        match self.apply_workspace_edit(preview.edit, &preview.what) {
            Ok(file_count) => {
                self.error = Some(format!(
                    "{}: applied to {file_count} file{}",
                    preview.what,
                    if file_count == 1 { "" } else { "s" }
                ));
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// The preview's Cancel button / window close: no I/O, the request
    /// already completed and was answered -- cancelling only declines to
    /// apply it (§3.5).
    fn cancel_refactor_preview(&mut self) {
        self.pending_refactor_preview = None;
    }

    /// The buffer line to paint the gutter lightbulb on, fed to
    /// `CodeEditor::code_action_line` (§2.3, §5). `None` unless
    /// `lsp.code_actions` is non-empty, the active tab's path matches
    /// `lsp.code_actions_target`'s (the answer is actually for this file),
    /// and that target's position converts to a valid line in the tab's
    /// *current* text.
    fn code_action_gutter_line(&self) -> Option<usize> {
        if self.lsp.code_actions.is_empty() {
            return None;
        }
        let idx = self.active_tab?;
        let path = self.tabs[idx].buffer.path()?;
        let (target_path, position) = self.lsp.code_actions_target.as_ref()?;
        if target_path != path {
            return None;
        }
        let text = self.tabs[idx].buffer.text();
        let offset = ide_lsp::position_to_byte_offset(text, *position)?;
        let offset = offset.min(text.len());
        Some(self.tabs[idx].buffer.text_buffer().lines().line_at(offset))
    }

    /// Called once per frame, alongside `sync_document_highlights`/
    /// `sync_code_actions`. Clears `git_gutter`/`git_gutter_path` (no
    /// active tab, an untitled tab, or a **dirty** buffer -- a dirty
    /// buffer's line numbers no longer necessarily match what `git.
    /// gutter_marks_for` would compute against the on-disk file,
    /// `docs/features/editor-git-gutter.md` §3.1), else recomputes them
    /// fresh every frame -- the same per-frame-recompute cost
    /// `git.show_working_tree_diff` already accepts for the Source
    /// Control view.
    fn sync_git_gutter(&mut self) {
        self.git_gutter.clear();
        self.git_gutter_path = None;
        let Some(idx) = self.active_tab else { return };
        if self.tabs[idx].buffer.is_dirty() {
            return;
        }
        let Some(path) = self.tabs[idx].buffer.path().map(Path::to_path_buf) else {
            return;
        };
        self.git_gutter = self.git.gutter_marks_for(&path);
        self.git_gutter_path = Some(path);
    }

    /// Called once per frame right after the editor widget's `show()`
    /// returns (§2.4). Opens the gutter's "Revert Hunk"/"Show Diff" popup
    /// on whatever line was clicked -- a click on a different mark just
    /// moves it, same "open on gesture, no explicit close required first"
    /// convention every other popup in this app already follows.
    fn handle_git_gutter_click(&mut self, output: &editor::EditorOutput) {
        if let Some(line) = output.git_gutter_clicked_line {
            self.git_gutter_popup_line = Some(line);
        }
    }

    /// The active tab's path, only while `git_gutter_popup_line` names a
    /// line that's actually still backed by fresh (non-stale) marks for
    /// that same path -- shared gating for `trigger_revert_hunk` and the
    /// popup's own render call.
    fn git_gutter_popup_target(&self) -> Option<(PathBuf, usize)> {
        let idx = self.active_tab?;
        let path = self.tabs[idx].buffer.path()?;
        if self.git_gutter_path.as_deref() != Some(path) {
            return None;
        }
        Some((path.to_path_buf(), self.git_gutter_popup_line?))
    }

    /// "Revert Hunk" button. No-op unless the popup is open for a line
    /// still backed by fresh marks -- builds `revert_hunk_change` from
    /// `git.hunks_for` and applies it as one undo step (`Buffer::apply`,
    /// never a direct disk write, §3.3). Closes the popup either way.
    fn trigger_revert_hunk(&mut self) {
        let target = self.git_gutter_popup_target();
        self.git_gutter_popup_line = None;
        let Some((path, line)) = target else {
            return;
        };
        let Some(idx) = self.active_tab else { return };
        let hunks = self.git.hunks_for(&path);
        let Some(change) =
            editor::revert_hunk_change(&hunks, line, self.tabs[idx].buffer.text_buffer())
        else {
            return;
        };
        let transaction = ide_core::text::Transaction::new(vec![change])
            .expect("a single change never overlaps itself");
        self.tabs[idx].buffer.apply(transaction);
    }

    /// "Show Diff" button: closes the popup, switches to the Source
    /// Control view, and loads the active tab's own working-tree diff --
    /// reuses that view wholesale rather than a hunk-scrolled sub-view
    /// (v1 simplification, §3.4).
    fn trigger_show_diff_for_gutter(&mut self) {
        self.git_gutter_popup_line = None;
        let Some(idx) = self.active_tab else { return };
        let Some(path) = self.tabs[idx].buffer.path().map(Path::to_path_buf) else {
            return;
        };
        self.view_mode = ViewMode::SourceControl;
        self.git.show_working_tree_diff(&path);
    }

    /// `ToggleBlameAnnotations` and the gutter context menu's "Annotate
    /// with Blame"/"Close Annotations" entry (`docs/features/
    /// git-branches-and-blame.md` §2.2.3): flips the active tab's blame
    /// off (dropping its cache back to `None`, immediately reclaiming the
    /// gutter lane) or on (populated from `GitRepo::blame_file` against
    /// the tab's last-saved path, same "diff/gutter only look at what's
    /// on disk" precedent `diff_file`/E7's gutter already establish).
    /// No-op for an untitled tab (`blame_file` needs a repo-relative path)
    /// -- there is nothing to toggle for a file with no path yet.
    fn toggle_blame_annotations(&mut self) {
        let Some(idx) = self.active_tab else { return };
        if self.tabs[idx].blame.is_some() {
            self.tabs[idx].blame = None;
            return;
        }
        let Some(path) = self.tabs[idx].buffer.path().map(Path::to_path_buf) else {
            return;
        };
        let lines = self.git.blame_for(&path);
        self.tabs[idx].blame = Some(editor::annotations_from_blame(&lines));
    }

    /// Refreshes `tab.blame` in place if it's currently on -- called after
    /// a successful Save and after a Reload (§3's edge cases), the same
    /// two triggers the git gutter's own marks already refresh on. A no-op
    /// when blame is off for this tab, or the tab has no path.
    fn refresh_blame_if_on(&mut self, idx: usize) {
        if self.tabs[idx].blame.is_none() {
            return;
        }
        let Some(path) = self.tabs[idx].buffer.path().map(Path::to_path_buf) else {
            return;
        };
        let lines = self.git.blame_for(&path);
        self.tabs[idx].blame = Some(editor::annotations_from_blame(&lines));
    }

    /// Called once per frame right after the editor widget's `show()`
    /// returns, alongside `handle_git_gutter_click` (§2.2.3's "primary
    /// entry point" is the gutter context menu, but a direct label click
    /// opens the popup too). Opens the blame popup on whatever line was
    /// clicked -- a click on a different annotation just moves it, the
    /// same "open on gesture, no explicit close required first" convention
    /// `handle_git_gutter_click` already follows.
    fn handle_blame_click(&mut self, idx: usize, output: &editor::EditorOutput) {
        let Some(line) = output.blame_clicked_line else {
            return;
        };
        let Some(annotations) = &self.tabs[idx].blame else {
            return;
        };
        if let Some(annotation) = annotations
            .iter()
            .find(|a| line >= a.line && line < a.line + a.run_len)
        {
            self.blame_popup_commit_id = Some(annotation.commit_id.clone());
        }
    }

    /// The branches popup's currently filtered/sorted rows (fuzzy-filtered
    /// by `branches_popup.filter`, score-descending -- same ordering
    /// `render_branches_popup` draws), as owned `(name, is_head)` pairs
    /// rather than `&BranchInfo`s specifically so this fn's return type
    /// carries no lifetime tied to `self` -- `filtered_commands` sidesteps
    /// the same problem for free since its registry is `'static`; this
    /// one isn't, so it clones instead. Shared by the render layer and
    /// `branches_popup_move_selection`/`_confirm` so keyboard nav and what
    /// the popup actually draws can never disagree about row order.
    fn filtered_branch_rows(&self) -> Vec<(String, bool)> {
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
                ide_core::fuzzy_score(filter, &name).map(|m| (m.score, (name, is_head)))
            })
            .collect();
        scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        scored.into_iter().map(|(_, row)| row).collect()
    }

    /// Branches popup-local list navigation, same exemption shape as
    /// `command_palette_move_selection`/`search_everywhere_move_selection`
    /// in `handle_shortcuts` -- widget-internal, not a registered command.
    fn branches_popup_move_selection(&mut self, delta: isize) {
        let count = self.filtered_branch_rows().len();
        if count == 0 {
            return;
        }
        let current = self.git.branches_popup.selected as isize;
        self.git.branches_popup.selected = (current + delta).rem_euclid(count as isize) as usize;
    }

    /// `Enter` on the branches popup: checks out the selected row, unless
    /// it's the branch already checked out (a no-op `Checkout` button is
    /// hidden entirely in the render layer for the same row, so `Enter`
    /// shouldn't do something the row's own buttons refuse to either).
    fn branches_popup_confirm(&mut self) {
        let Some(root) = self.project.as_ref().map(|p| p.root().to_path_buf()) else {
            return;
        };
        let rows = self.filtered_branch_rows();
        let Some((name, is_head)) = rows.get(self.git.branches_popup.selected).cloned() else {
            return;
        };
        if is_head {
            return;
        }
        if let Err(e) = self.git.checkout_branch(&root, &name) {
            self.error = Some(e);
        }
    }

    /// Shared apply primitive, extracted from `handle_workspace_edit_ready`'s
    /// original body with no behavioural change to that caller: partitions
    /// `edit.edits` by whether an open tab already has that file, applies
    /// the disk subset first via `ide_core::apply_workspace_edit_to_disk`
    /// (all-or-nothing; a failure here means the buffer subset never runs
    /// at all), then -- only once the disk subset has fully succeeded --
    /// applies the buffer subset via `Buffer::apply`. `what` names the
    /// operation for its own error strings (e.g. `"Code action"`,
    /// `` "Rename to `count`" ``); on success, returns the number of files
    /// touched so the caller can build its own one-line summary
    /// (`docs/features/rename-refactoring.md` §2.3). Reused by
    /// `handle_workspace_edit_ready`, `handle_rename_ready`'s direct-apply
    /// path, and the rename preview's Apply button.
    fn apply_workspace_edit(
        &mut self,
        edit: ide_lsp::WorkspaceEdit,
        what: &str,
    ) -> Result<usize, String> {
        let mut file_edits: Vec<ide_core::FileEdit> = Vec::with_capacity(edit.edits.len());
        for file_edit in &edit.edits {
            let open_tab = self
                .tabs
                .iter()
                .position(|tab| tab.buffer.path() == Some(file_edit.path.as_path()));
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
            file_edits.push(ide_core::FileEdit {
                path: file_edit.path.clone(),
                transaction,
            });
        }
        self.apply_file_edits(file_edits, what)
    }

    /// Shared apply primitive, extracted from `apply_workspace_edit`'s
    /// original body with no behavioural change to that caller
    /// (`docs/features/search-in-path-v2.md` §2.2/§4): partitions `edits`
    /// by whether an open tab already has that file, applies the disk
    /// subset first via `ide_core::apply_workspace_edit_to_disk`
    /// (all-or-nothing; a failure here means the buffer subset never runs
    /// at all), then -- only once the disk subset has fully succeeded --
    /// applies the buffer subset via `Buffer::apply`. `what` names the
    /// operation for its own error strings; on success, returns the number
    /// of files touched. Reused by `apply_workspace_edit` (after its own
    /// `ide_lsp::TextEdit` -> `ide_core::text::Transaction` conversion) and
    /// `confirm_replace_in_path_preview` (whose edits are already
    /// `ide_core::FileEdit`s, no LSP conversion needed).
    fn apply_file_edits(
        &mut self,
        edits: Vec<ide_core::FileEdit>,
        what: &str,
    ) -> Result<usize, String> {
        let mut disk_edits: Vec<ide_core::FileEdit> = Vec::new();
        let mut buffer_edits: Vec<(usize, ide_core::text::Transaction)> = Vec::new();

        for file_edit in edits {
            let open_tab = self
                .tabs
                .iter()
                .position(|tab| tab.buffer.path() == Some(file_edit.path.as_path()));
            match open_tab {
                Some(idx) => buffer_edits.push((idx, file_edit.transaction)),
                None => disk_edits.push(file_edit),
            }
        }

        let file_count = disk_edits.len() + buffer_edits.len();

        if !disk_edits.is_empty() {
            let workspace_edit = ide_core::WorkspaceEdit { edits: disk_edits };
            if let Err(e) = ide_core::apply_workspace_edit_to_disk(&workspace_edit) {
                return Err(format!("{what}: {e}"));
            }
        }

        for (idx, transaction) in buffer_edits {
            self.tabs[idx].buffer.apply(transaction);
        }

        Ok(file_count)
    }

    /// Called once per frame, immediately after `self.lsp.poll()`, alongside
    /// `handle_goto_response` (`docs/features/code-actions.md` §2.3, §3.4).
    /// No-op unless `self.lsp.workspace_edit_ready` is set this frame. Sets
    /// `self.error` to a one-line summary either way.
    fn handle_workspace_edit_ready(&mut self) {
        if !self.lsp.workspace_edit_ready {
            return;
        }
        // Unconditional take-and-reset on every real event this method
        // processes, not only ones with a usable edit, so a stray `true`
        // can never leak into a later, unrelated apply
        // (`docs/features/refactor-this.md` §3.4).
        let via_preview = std::mem::take(&mut self.via_refactor_preview);
        let what = self
            .lsp
            .workspace_edit_label
            .clone()
            .unwrap_or_else(|| "Code action".to_string());
        let Some(edit) = self.lsp.workspace_edit.take() else {
            self.error = Some(format!("{what}: nothing to apply"));
            return;
        };

        if via_preview {
            self.show_refactor_preview(what, edit);
            return;
        }

        let file_count = match self.apply_workspace_edit(edit, &what) {
            Ok(n) => n,
            Err(e) => {
                self.error = Some(e);
                return;
            }
        };

        self.error = Some(format!(
            "{what}: applied to {file_count} file{}",
            if file_count == 1 { "" } else { "s" }
        ));
    }

    /// `⌘⌥L`'s entry point (`docs/features/formatting.md` §2.3, §3.1). No-op
    /// with no active tab, or an active tab with no path (untitled buffer --
    /// nothing on disk/server to format against). `tab_size`/`insert_spaces`
    /// come from the tab's already-resolved `IndentUnit`, the same value
    /// auto-indent and `Tab` already use.
    fn trigger_reformat_code(&mut self) {
        let Some(idx) = self.active_tab else {
            return;
        };
        let Some(path) = self.tabs[idx].buffer.path() else {
            return;
        };
        let path = path.to_path_buf();
        let unit = self.tabs[idx].editor.indent();
        let insert_spaces = matches!(unit.style, IndentStyle::Spaces);
        self.lsp
            .request_format(&path, unit.width as u32, insert_spaces);
    }

    /// `try_save_active`'s Format-on-Save follow-up
    /// (`docs/features/formatting.md` §3.4): called only after the
    /// synchronous save itself has already fully succeeded, completely
    /// unchanged and un-delayed by anything here. No-op unless
    /// `self.format_on_save` and the active tab has a path -- no capability
    /// check needed, `request_format` is already fail-closed and always
    /// resolves (§2.3, §4).
    fn maybe_trigger_format_on_save(&mut self) {
        if !self.format_on_save {
            return;
        }
        let Some(idx) = self.active_tab else {
            return;
        };
        let Some(path) = self.tabs[idx].buffer.path() else {
            return;
        };
        let path = path.to_path_buf();
        self.trigger_reformat_code();
        self.format_on_save_target = Some(path);
    }

    /// Called once per frame, right after `self.lsp.poll()`, alongside
    /// `handle_goto_response`/`handle_workspace_edit_ready`
    /// (`docs/features/formatting.md` §2.3, §3.4). No-op unless
    /// `self.lsp.format_ready`. Finds the open tab whose path matches the
    /// response's `format_path` (§4's invariant guarantees one exists
    /// unless the tab was closed between the request and this response, a
    /// TOCTOU race handled below). On `Some(edit)` with a matching tab:
    /// applies it via `Buffer::apply` -- no disk-subset phase, unlike
    /// `handle_workspace_edit_ready`, since a formatting edit is only ever
    /// requested for a file that's already open in a tab. If
    /// `self.format_on_save_target` matches this response's path,
    /// additionally saves that exact tab by index via `save_tab_at`
    /// (**never** `save_active`/`self.active_tab`, so a tab switch between
    /// the save and this response can never save the wrong tab). No
    /// matching tab, or `edit: None`, is a no-op beyond clearing
    /// `format_on_save_target` when it matches.
    fn handle_format_ready(&mut self) {
        if !self.lsp.format_ready {
            return;
        }
        // Consumed here, not reset by `poll()` -- see `LspBridge::format_ready`'s
        // doc comment for why: a same-frame synchronous no-client self-resolve
        // (set by `request_format` from inside `handle_shortcuts`, before
        // `poll()` runs) must survive to be read here.
        self.lsp.format_ready = false;
        let Some(path) = self.lsp.format_path.clone() else {
            return;
        };
        let edit = self.lsp.format_edit.take();
        let target_matches = self.format_on_save_target.as_deref() == Some(path.as_path());

        let idx = self
            .tabs
            .iter()
            .position(|tab| tab.buffer.path() == Some(path.as_path()));
        if let (Some(idx), Some(edit)) = (idx, edit) {
            let Some(file_edit) = edit.edits.into_iter().find(|e| e.path == path) else {
                if target_matches {
                    self.format_on_save_target = None;
                }
                return;
            };
            let text = self.tabs[idx].buffer.text().to_string();
            if let Some(transaction) =
                workspace_text_edits_to_transaction(&text, &file_edit.text_edits)
            {
                self.tabs[idx].buffer.apply(transaction);
                if target_matches {
                    if let Some(Err(e)) = self.save_tab_at(idx) {
                        self.error = Some(e.to_string());
                    }
                }
            } else {
                self.error = Some(format!(
                    "Reformat Code: an edit for {} does not fit its current content",
                    path.display()
                ));
            }
        }
        if target_matches {
            self.format_on_save_target = None;
        }
    }

    /// `⇧F6`'s entry point (`docs/features/rename-refactoring.md` §3.1).
    /// No-op if there's no active tab, or the active tab's buffer has no
    /// path (untitled buffer -- same gating `ReformatCode` already uses).
    /// No-op with `self.error` set if there's no running language server,
    /// or the caret isn't on a symbol. Otherwise opens the popup
    /// immediately, prefilled with the word under the caret, and fires
    /// `PrepareRename` ambiently in parallel -- not gating the popup.
    fn trigger_rename(&mut self) {
        let Some(idx) = self.active_tab else {
            return;
        };
        let Some(path) = self.tabs[idx].buffer.path() else {
            return;
        };
        let path = path.to_path_buf();
        if !self.lsp.is_running_for(&path) {
            self.error = Some("Rename: no language server is running".to_string());
            return;
        }
        let Some(offset) = self.active_cursor_offset else {
            self.error = Some("Rename: no symbol under the caret".to_string());
            return;
        };
        let text = self.tabs[idx].buffer.text();
        let Some(local_range) = editor::word_range_at(text, offset) else {
            self.error = Some("Rename: no symbol under the caret".to_string());
            return;
        };
        let original_name = text[local_range.clone()].to_string();
        let Some(position) = ide_lsp::byte_offset_to_position(text, local_range.start) else {
            self.error = Some("Rename: no symbol under the caret".to_string());
            return;
        };

        self.rename_popup = Some(RenamePopup {
            path: path.clone(),
            position,
            original_name: original_name.clone(),
            input: original_name,
        });
        self.pending_rename_focus = true;
        self.lsp.request_prepare_rename(&path, position);
    }

    /// The popup's Enter/confirm action (§3.3). Closes the popup
    /// immediately, regardless of outcome. Sends nothing, shows no
    /// message, if the typed name is empty or unchanged from the original
    /// (JetBrains itself treats confirming with an unchanged name as a
    /// silent cancel).
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

    /// The popup's Escape/Cancel action: closes it, nothing else.
    fn cancel_rename(&mut self) {
        self.rename_popup = None;
    }

    /// Called once per frame, alongside `handle_goto_response`/
    /// `handle_format_ready` (§3.2). No-op unless
    /// `self.lsp.prepare_rename_ready`. `PrepareRenameReady`'s
    /// `renameable` is never a hard gate -- the popup always opens on
    /// trigger regardless of server support (§4); the only effect here is
    /// closing it early on an explicit `renameable: false` from a server
    /// that does support the check, while the popup is still open for the
    /// same `(path, position)` this response answers (the same "does this
    /// response still answer what's currently open" check
    /// `format_on_save_target` matching already establishes in
    /// `formatting.md`) -- a stale response for an already-closed or
    /// already-superseded popup is a no-op.
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
                self.error = Some("Rename: this element cannot be renamed".to_string());
            }
        }
    }

    /// Called once per frame, alongside the above (§3.4). No-op unless
    /// `self.lsp.rename_ready`. `edit: None` reports nothing to apply. A
    /// single-file edit whose one file is the *currently* active tab's
    /// path (re-read fresh here, not the popup's stale target -- the
    /// active tab may have changed since the request was sent, the same
    /// "never trust a stale index/tab" discipline `formatting.md`'s
    /// `save_tab_at`-by-path-not-by-active-tab already established)
    /// applies immediately. Anything else (more than one file, the one
    /// file isn't the current tab's, or there is currently no active tab
    /// at all) escalates to the preview instead.
    fn handle_rename_ready(&mut self) {
        if !self.lsp.rename_ready {
            return;
        }
        let Some(edit) = self.lsp.rename_edit.take() else {
            let new_name = self.lsp.rename_new_name.take().unwrap_or_default();
            self.error = Some(format!("Rename to `{new_name}`: nothing to apply"));
            return;
        };
        let new_name = self.lsp.rename_new_name.take().unwrap_or_default();
        let what = format!("Rename to `{new_name}`");

        let active_path = self.active_tab.and_then(|idx| self.tabs[idx].buffer.path());
        let applies_directly = match edit.edits.as_slice() {
            [only] => active_path == Some(only.path.as_path()),
            _ => false,
        };

        if applies_directly {
            match self.apply_workspace_edit(edit, &what) {
                Ok(file_count) => {
                    self.error = Some(format!(
                        "{what}: applied to {file_count} file{}",
                        if file_count == 1 { "" } else { "s" }
                    ));
                }
                Err(e) => self.error = Some(e),
            }
        } else {
            self.pending_rename_preview = Some((edit, new_name));
        }
    }

    /// Called from the same two sites `notify_lsp_changed` already fires
    /// from, right after it (§3.6): once when a tab's `DidOpen` is sent,
    /// and once every time `notify_lsp_changed` actually sends a
    /// `DidChange` (gated by the editor widget's own `changed` output, same
    /// rate limiting that already protects `DidChange` notifications).
    /// Always requests the whole document (§4) -- v1 doesn't scope to the
    /// visible viewport.
    fn sync_inlay_hints(&mut self, idx: usize) {
        let tab = &self.tabs[idx];
        let Some(path) = tab.buffer.path() else {
            return;
        };
        let text = tab.buffer.text();
        let Some(end) = ide_lsp::byte_offset_to_position(text, text.len()) else {
            return;
        };
        let path = path.to_path_buf();
        self.lsp.request_inlay_hints(
            &path,
            ide_lsp::Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end,
            },
        );
    }

    /// The active tab's entry in `self.lsp.inlay_hints`, or `&[]` if
    /// there's no active tab or no entry yet (brand new tab, or a query
    /// still in flight). Read by `render_tabs_and_editor` to feed
    /// `CodeEditor::inlay_hints` (§2.2).
    fn active_inlay_hints(&self) -> &[InlayHint] {
        self.active_tab
            .and_then(|idx| self.tabs[idx].buffer.path())
            .and_then(|path| self.lsp.inlay_hints.get(path))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Called from the same two sites `sync_inlay_hints` already fires
    /// from, immediately alongside it (`docs/features/
    /// semantic-highlighting.md` §3.3): once on a tab's `DidOpen`, once on
    /// every actual (not idle-frame) `DidChange`. Always the whole
    /// document -- there is no viewport-scoped variant.
    fn sync_semantic_tokens(&mut self, idx: usize) {
        let tab = &self.tabs[idx];
        let Some(path) = tab.buffer.path() else {
            return;
        };
        let path = path.to_path_buf();
        self.lsp.request_semantic_tokens(&path);
    }

    /// The active tab's entry in `self.lsp.semantic_tokens`, or `&[]` if
    /// there's no active tab or no entry yet -- same shape as
    /// `active_inlay_hints`. Read by `render_tabs_and_editor` to feed
    /// `CodeEditor::semantic_tokens`.
    fn active_semantic_tokens(&self) -> &[ide_lsp::SemanticToken] {
        self.active_tab
            .and_then(|idx| self.tabs[idx].buffer.path())
            .and_then(|path| self.lsp.semantic_tokens.get(path))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Whether `Esc` belongs to the editor this frame -- true exactly when
    /// the visible editor has more than one cursor and is about to collapse
    /// them. `handle_shortcuts` runs before any panel is drawn, so the
    /// Usages popup has to yield the key here rather than waiting for the
    /// editor to consume it (`multiple-cursors.md` §3.6).
    fn editor_owns_escape(&self) -> bool {
        self.active_tab
            .filter(|_| self.view_mode == ViewMode::Editor)
            .is_some_and(|idx| {
                self.tabs[idx]
                    .buffer
                    .text_buffer()
                    .selections()
                    .is_multiple()
            })
    }

    /// `path` shortened against the open project's root for display --
    /// falls back to the full path when there's no project or the path
    /// lies outside it (which `LspBridge::references` entries never do,
    /// having been validated in `ide-lsp`).
    fn display_path(&self, path: &Path) -> String {
        self.project
            .as_ref()
            .and_then(|project| path.strip_prefix(project.root()).ok())
            .unwrap_or(path)
            .display()
            .to_string()
    }

    /// `"Declaration"` / `"Type Declaration"` / `"Implementation"` for
    /// whatever `self.goto_action` currently holds -- the Go to popup's
    /// title/empty-state wording (`docs/features/goto-definition.md` §2.2).
    /// `"Declaration"` if `None`, which the popup never actually observes:
    /// `handle_goto_response` only opens it once `goto_action` has just
    /// been set by the trigger that led here.
    fn goto_action_label(&self) -> &'static str {
        match self.goto_action {
            Some(GotoKind::Definition) | None => "Declaration",
            Some(GotoKind::TypeDefinition) => "Type Declaration",
            Some(GotoKind::Implementation) => "Implementation",
        }
    }

    /// The current find-usages results, grouped by file and ordered within
    /// each file -- shared by the bottom panel and the popup so the two
    /// can't drift into different orderings. Cloned rather than borrowed:
    /// both callers render inside a closure that also needs `&mut self`.
    fn sorted_references(&self) -> Vec<Location> {
        let mut entries = self.lsp.references.clone();
        entries.sort_by(|a, b| {
            a.path.cmp(&b.path).then(
                (a.range.start.line, a.range.start.character)
                    .cmp(&(b.range.start.line, b.range.start.character)),
            )
        });
        entries
    }

    /// The Go to popup's ordering -- same shape as `sorted_references`,
    /// applied to `self.lsp.goto` instead
    /// (`docs/features/goto-definition.md` §2.2).
    fn sorted_goto(&self) -> Vec<Location> {
        let mut entries = self.lsp.goto.clone();
        entries.sort_by(|a, b| {
            a.path.cmp(&b.path).then(
                (a.range.start.line, a.range.start.character)
                    .cmp(&(b.range.start.line, b.range.start.character)),
            )
        });
        entries
    }

    /// Parses `search_include_text`/`search_exclude_text` into `search_
    /// options.include`/`exclude` -- called once at the top of `run_search`/
    /// `run_replace_preview`, i.e. only when the user actually submits a
    /// request, never per-frame (see `search_include_text`'s doc comment).
    fn sync_search_glob_options(&mut self) {
        self.search_options.include = split_glob_list(&self.search_include_text);
        self.search_options.exclude = split_glob_list(&self.search_exclude_text);
    }

    /// Toolbar "Search" button / `Cmd+Shift+F` / Enter-in-query-field entry
    /// point (`search-in-path-v2.md` §2.2/§3): no-op if no project is
    /// open or `search_query` is empty after trimming; otherwise parses the
    /// Include/Exclude text fields, clones `self.tree` (already cloned
    /// every frame for the tree panel -- §4) and hands it, plus a clone of
    /// `search_options`, to `PathSearchPanel::run`, which itself no-ops if
    /// a search is already in flight.
    fn run_search(&mut self) {
        if self.search_query.trim().is_empty() {
            return;
        }
        let Some(tree) = self.tree.clone() else {
            return;
        };
        self.sync_search_glob_options();
        self.search
            .run(tree, self.search_query.clone(), self.search_options.clone());
    }

    /// `run_search` plus switching the bottom panel to the Search view --
    /// shared by all three trigger mechanisms (toolbar button,
    /// `Cmd+Shift+F`, Enter in the query field; doc §3).
    fn trigger_search(&mut self) {
        self.run_search();
        self.bottom_view = BottomView::Search;
    }

    /// `⌘⇧R` / "Replace in Path" entry point (doc §2.2/§3.3): opens the
    /// Search view and reveals the replacement field -- mirrors
    /// `FindBar::open`'s `replace_open` convention (never turns
    /// `search_replace_open` back off on its own). Does **not** itself
    /// compute a preview; that's the panel's own "Preview" button,
    /// `run_replace_preview`.
    fn trigger_replace_in_path(&mut self) {
        self.search_replace_open = true;
        self.bottom_view = BottomView::Search;
    }

    /// The panel's "Preview" button (doc §2.2/§3.3): no-op on an empty
    /// query, empty replacement, or no project -- otherwise parses the
    /// Include/Exclude text fields, clones the tree, and hands it to
    /// `PathSearchPanel::run_replace`, which itself no-ops if a preview is
    /// already being computed.
    fn run_replace_preview(&mut self) {
        if self.search_query.trim().is_empty() || self.search_replacement.is_empty() {
            return;
        }
        let Some(tree) = self.tree.clone() else {
            return;
        };
        self.sync_search_glob_options();
        self.search.run_replace(
            tree,
            self.search_query.clone(),
            self.search_replacement.clone(),
            self.search_options.clone(),
        );
    }

    /// Builds `pending_replace_in_path_preview` from a ready
    /// `ide_core::WorkspaceEdit` (doc §3.3): for each file, reads old text
    /// (open tab's buffer if any, else a fresh disk read -- same source
    /// `show_refactor_preview` already reads from) -- deliberately re-read
    /// at preview-build time rather than trusting whatever text
    /// `replace_in_path` itself read moments earlier off-thread, since a
    /// file could have changed on disk in between -- computes the post-edit
    /// text via `ide_core::apply_transaction`, and diffs old/new via
    /// `ide_core::diff_text`. Read-only -- never touches disk or any
    /// buffer. Unlike `show_refactor_preview`, no LSP `TextEdit`-to-
    /// `Transaction` conversion is needed: `FileEdit::transaction` is
    /// already an `ide_core::text::Transaction`.
    fn show_replace_in_path_preview(&mut self, edit: ide_core::WorkspaceEdit) {
        let diffs = edit
            .edits
            .iter()
            .map(|file_edit| {
                let open_tab = self
                    .tabs
                    .iter()
                    .position(|tab| tab.buffer.path() == Some(file_edit.path.as_path()));
                let old_text = match open_tab {
                    Some(idx) => self.tabs[idx].buffer.text().to_string(),
                    None => std::fs::read_to_string(&file_edit.path).ok()?,
                };
                let new_text = ide_core::apply_transaction(&old_text, &file_edit.transaction)?;
                ide_core::diff_text(&file_edit.path, &old_text, &new_text)
            })
            .collect();
        self.pending_replace_in_path_preview = Some(ReplaceInPathPreview { edit, diffs });
    }

    /// The preview's Apply button (doc §3.3): calls the newly extracted
    /// `apply_file_edits` directly with `preview.edit.edits` -- no LSP
    /// conversion needed, unlike `confirm_refactor_preview`.
    fn confirm_replace_in_path_preview(&mut self) {
        let Some(preview) = self.pending_replace_in_path_preview.take() else {
            return;
        };
        match self.apply_file_edits(preview.edit.edits, "Replace in Path") {
            Ok(file_count) => {
                self.error = Some(format!(
                    "Replace in Path: applied to {file_count} file{}",
                    if file_count == 1 { "" } else { "s" }
                ));
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// The preview's Cancel button / window close: no I/O, the request
    /// already completed and was answered -- cancelling only declines to
    /// apply it, same as `cancel_refactor_preview`.
    fn cancel_replace_in_path_preview(&mut self) {
        self.pending_replace_in_path_preview = None;
    }

    /// "Add" button in the Languages… settings window (doc §3): trims all
    /// three required draft fields; rejects (sets `language_settings_error`,
    /// leaves `custom_languages` and the draft fields untouched) if any is
    /// empty after trimming, or if the extension (leading `.` stripped,
    /// case-insensitive) collides with the built-in Rust config's `"rs"` or
    /// an existing `custom_languages` entry. `new_language_args` is
    /// optional: split on whitespace into individual argv entries
    /// (`docs/features/language-server-arguments.md` §2.3 -- naive
    /// splitting, no quoting support; an empty/all-whitespace field parses
    /// to `vec![]`, matching "no arguments needed" rather than an error).
    /// On success: pushes the new config, clears the draft fields and
    /// `language_settings_error`, and re-runs `resync_active_languages` so a
    /// matching language added while its project is already open takes
    /// effect immediately.
    fn add_custom_language(&mut self) {
        let name = self.new_language_name.trim().to_string();
        let extension = self
            .new_language_extension
            .trim()
            .trim_start_matches('.')
            .to_string();
        let command = self.new_language_command.trim().to_string();
        let args: Vec<String> = self
            .new_language_args
            .split_whitespace()
            .map(str::to_string)
            .collect();

        if name.is_empty() || extension.is_empty() || command.is_empty() {
            self.language_settings_error =
                Some("Name, extension, and command are all required.".to_string());
            return;
        }
        if extension.eq_ignore_ascii_case("rs")
            || self
                .custom_languages
                .iter()
                .any(|c| c.extension.eq_ignore_ascii_case(&extension))
        {
            self.language_settings_error =
                Some(format!("Extension \".{extension}\" is already in use."));
            return;
        }

        self.custom_languages.push(LanguageConfig {
            name,
            extension,
            command,
            args,
            extra_extensions: Vec::new(),
        });
        self.new_language_name.clear();
        self.new_language_extension.clear();
        self.new_language_command.clear();
        self.new_language_args.clear();
        self.language_settings_error = None;
        self.resync_active_languages();
    }

    /// "Remove" button in the Languages… settings window (doc §3):
    /// silently no-ops if `index` is out of bounds (the same defensive
    /// convention `close_tab_now`'s existing bounds guard already uses),
    /// otherwise removes the entry and re-runs detection the same way
    /// `add_custom_language` does.
    fn remove_custom_language(&mut self, index: usize) {
        if index >= self.custom_languages.len() {
            return;
        }
        self.custom_languages.remove(index);
        self.resync_active_languages();
    }

    /// Notifies the LSP client that a tab's text changed, via
    /// `LspRequest::DidChange`. Gated by the editor widget's `changed`
    /// output (doc §2.2) so an idle tab doesn't flood the server with no-op
    /// notifications every frame.
    fn notify_lsp_changed(&mut self, idx: usize) {
        let tab = &mut self.tabs[idx];
        if let Some(path) = tab.buffer.path() {
            let text = tab.buffer.text().to_string();
            self.lsp.send(
                path,
                LspRequest::DidChange {
                    path: path.to_path_buf(),
                    text,
                },
            );
        }
    }

    /// Copies each open tab's current diagnostics out of `LspBridge`'s
    /// workspace-wide map every frame (doc §3) -- call after `lsp.poll()`.
    fn sync_tab_diagnostics(&mut self) {
        for tab in &mut self.tabs {
            tab.diagnostics = tab
                .buffer
                .path()
                .and_then(|p| self.lsp.diagnostics.get(p))
                .cloned()
                .unwrap_or_default();
        }
    }

    fn new_untitled_tab(&mut self) {
        self.untitled_count += 1;
        let title = if self.untitled_count == 1 {
            "Untitled".to_string()
        } else {
            format!("Untitled-{}", self.untitled_count)
        };
        self.tabs.push(Tab::untitled(title));
        self.active_tab = Some(self.tabs.len() - 1);
    }

    fn first_dirty_tab(&self) -> Option<usize> {
        self.tabs.iter().position(|t| t.buffer.is_dirty())
    }

    /// `NextTab`/`PreviousTab` (`docs/roadmap.md` §5.2: `⌘⇧]`/`⌘⇧[`).
    /// Wraps at either end since JetBrains' tab cycling does too.
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

    fn request_close_tab(&mut self, idx: usize) {
        if self.tabs.get(idx).is_some_and(|t| t.buffer.is_dirty()) {
            self.pending_confirm = Some(PendingConfirm::CloseTab(idx));
        } else {
            self.close_tab_now(idx);
        }
    }

    fn close_tab_now(&mut self, idx: usize) {
        if idx >= self.tabs.len() {
            return;
        }
        if let Some(path) = self.tabs[idx].buffer.path() {
            self.lsp.send(
                path,
                LspRequest::DidClose {
                    path: path.to_path_buf(),
                },
            );
        }
        self.tabs.remove(idx);
        self.active_tab = match self.active_tab {
            _ if self.tabs.is_empty() => None,
            Some(active) if active > idx => Some(active - 1),
            Some(active) if active == idx => Some(idx.min(self.tabs.len() - 1)),
            other => other,
        };
    }

    /// Returns `true` if it's OK to quit immediately (no dirty tabs).
    /// Otherwise arms the confirm-discard flow and returns `false` — the
    /// caller must cancel the pending OS close request.
    fn request_quit(&mut self) -> bool {
        if self.first_dirty_tab().is_some() {
            self.pending_confirm = Some(PendingConfirm::Quit);
            false
        } else {
            true
        }
    }

    fn confirm_discard(&mut self) {
        match self.pending_confirm.take() {
            Some(PendingConfirm::CloseTab(idx)) => self.close_tab_now(idx),
            Some(PendingConfirm::Quit) => {
                if let Some(idx) = self.first_dirty_tab() {
                    self.close_tab_now(idx);
                }
                if self.first_dirty_tab().is_some() {
                    self.pending_confirm = Some(PendingConfirm::Quit);
                } else {
                    self.should_quit = true;
                }
            }
            None => {}
        }
    }

    fn cancel_confirm(&mut self) {
        self.pending_confirm = None;
    }

    fn undo_active(&mut self) {
        if let Some(idx) = self.active_tab {
            if self.tabs[idx].buffer.undo() {
                self.notify_lsp_changed(idx);
            }
        }
    }

    fn redo_active(&mut self) {
        if let Some(idx) = self.active_tab {
            if self.tabs[idx].buffer.redo() {
                self.notify_lsp_changed(idx);
            }
        }
    }

    /// `Some(Ok/Err)` if the active tab already has a path (saved
    /// directly); `None` if the active tab is untitled (caller must show
    /// a Save As dialog) or there is no active tab.
    fn save_active(&mut self) -> Option<Result<(), ide_core::BufferError>> {
        self.active_tab.and_then(|idx| self.save_tab_at(idx))
    }

    /// The by-index primitive `save_active` delegates to. Introduced so
    /// `handle_format_ready`'s Format-on-Save follow-up save
    /// (`docs/features/formatting.md` §2.3, §3.4, §4) can target the tab
    /// that was actually reformatted, never whatever tab happens to be
    /// active by the time the response lands. `Some(Ok/Err)` if `idx`'s
    /// tab already has a path; `None` if it's untitled.
    fn save_tab_at(&mut self, idx: usize) -> Option<Result<(), ide_core::BufferError>> {
        let path = self.tabs[idx].buffer.path()?.to_path_buf();
        // `file-watcher.md` §3.3: suppress before the write, not after --
        // decide what's about to happen, then do it, the same ordering
        // `EditorConfig`'s own save sequence already establishes below.
        if let Some(watcher) = self.watcher.as_mut() {
            watcher.suppress(&path);
        }
        let tab = &mut self.tabs[idx];
        let outcome = Self::save_tab_with_config(tab);
        let result = self.finish_save(outcome);
        if result.is_ok() {
            self.refresh_blame_if_on(idx);
        }
        Some(result)
    }

    fn save_active_as(&mut self, path: &Path) -> Option<Result<(), ide_core::BufferError>> {
        let idx = self.active_tab?;
        // Canonicalized for the same reason `open_file` canonicalizes its
        // incoming path (`file-watcher.md` §3.4's "Path identity"
        // invariant) -- `Buffer::save_as` is the other place a path enters
        // `Tab` state besides `open_file`, and a native save dialog's
        // result is no more guaranteed canonical than an open dialog's.
        let canonical = Self::canonicalize_best_effort(path);
        let path = canonical.as_path();
        // Resolution re-runs after Save As: the new path may sit under
        // different rules (§3.6). Computed before borrowing `self.tabs[idx]`
        // mutably -- `resolve_editor_config` only needs `self.project`.
        let config = self.resolve_editor_config(path);
        if let Some(watcher) = self.watcher.as_mut() {
            watcher.suppress(path);
        }
        let tab = &mut self.tabs[idx];
        tab.apply_editor_config(config);
        // `Buffer` has no charset-aware `save_as`; establish the path with a
        // plain write first (needed either way -- an untitled buffer has no
        // path for `save_edit`'s own write to land on yet), then the shared
        // sequence below immediately rewrites it correctly under the
        // resolved properties and charset.
        let outcome = tab
            .buffer
            .save_as(path)
            .and_then(|()| Self::save_tab_with_config(tab));
        let result = self.finish_save(outcome);
        if result.is_ok() {
            let tab = &mut self.tabs[idx];
            tab.title = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| tab.title.clone());
            self.refresh_blame_if_on(idx);
        }
        Some(result)
    }

    /// The doc §3.6 save sequence: apply the minimal `save_edit` transaction
    /// first (undoable, carries every cursor through `Selections::map`),
    /// then write under the resolved charset. Shared by `save_active` and
    /// (after establishing the new path) `save_active_as`. `Ok`'s payload is
    /// the one-line charset notice (§3.6's "Charset" paragraph) the first
    /// time this tab's config names a charset the buffer can't represent --
    /// `None` on every later save of the same tab, or when the config names
    /// no charset or one `save_with` can actually honour.
    fn save_tab_with_config(tab: &mut Tab) -> Result<Option<String>, ide_core::BufferError> {
        if let Some(edit) = editorconfig::save_edit(tab.buffer.text(), &tab.config) {
            tab.buffer.apply(edit);
        }
        tab.buffer
            .save_with(editorconfig::save_charset(&tab.config))?;
        let notice = if !tab.charset_notice_shown
            && matches!(
                tab.config.charset,
                Some(Charset::Latin1 | Charset::Utf16Le | Charset::Utf16Be)
            ) {
            tab.charset_notice_shown = true;
            Some(format!(
                "{} was saved as UTF-8: {:?} from .editorconfig isn't supported",
                tab.title,
                tab.config.charset.expect("just matched Some above"),
            ))
        } else {
            None
        };
        Ok(notice)
    }

    /// Surfaces `save_tab_with_config`'s notice through the app's existing
    /// one-line message field and collapses its `Result<Option<String>, _>`
    /// back to the plain `Result<(), _>` every caller's return type expects.
    fn finish_save(
        &mut self,
        outcome: Result<Option<String>, ide_core::BufferError>,
    ) -> Result<(), ide_core::BufferError> {
        match outcome {
            Ok(notice) => {
                if let Some(message) = notice {
                    self.error = Some(message);
                }
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Drains `self.watcher.poll()` and dispatches every event
    /// (`file-watcher.md` §3.4, §3.5). Called once per frame from
    /// `App::ui`, after every other per-frame poll (`lsp.poll`,
    /// `search.poll`, `cargo.poll`) so it follows the same "poll
    /// everything, then paint" shape. No-op if no project is open. Returns
    /// whether any event was dispatched, so the caller can
    /// `ctx.request_repaint()` the same way it does for every other poll --
    /// `eframe::NativeOptions::default()` doesn't repaint continuously, so
    /// without this a background change would sit unseen until unrelated
    /// input (mouse move, keystroke) next woke the frame loop.
    fn poll_watcher(&mut self) -> bool {
        let Some(watcher) = self.watcher.as_mut() else {
            return false;
        };
        let events = watcher.poll();
        let dispatched = !events.is_empty();
        for event in events {
            match event {
                WatchEvent::TreeChanged => self.refresh_tree(),
                WatchEvent::FileModified(path) => self.handle_external_modification(&path),
                WatchEvent::FileRemoved(path) => self.handle_external_removal(&path),
            }
        }
        dispatched
    }

    /// §3.4: a clean tab reloads silently (nothing of the user's to lose);
    /// a dirty tab gets a `Modified` notice instead, left for the user to
    /// resolve via `reload_active_from_disk`/`dismiss_external_change`.
    fn handle_external_modification(&mut self, path: &Path) {
        let Some(idx) = self.tabs.iter().position(|t| t.buffer.path() == Some(path)) else {
            return;
        };
        if self.tabs[idx].buffer.is_dirty() {
            self.tabs[idx].external_change = Some(ExternalChange::Modified);
        } else {
            self.reload_tab_from_disk(idx);
        }
    }

    /// §3.5: regardless of dirty state -- unlike a content change, there's
    /// no "nothing to lose" case for a file that's simply gone.
    fn handle_external_removal(&mut self, path: &Path) {
        if let Some(idx) = self.tabs.iter().position(|t| t.buffer.path() == Some(path)) {
            self.tabs[idx].external_change = Some(ExternalChange::Deleted);
        }
    }

    /// Shared by the silent auto-reload (`handle_external_modification`,
    /// any tab, not necessarily the active one) and the explicit user
    /// "Reload" action (`reload_active_from_disk`, always the active tab).
    fn reload_tab_from_disk(&mut self, idx: usize) {
        let Some(path) = self.tabs[idx].buffer.path().map(Path::to_path_buf) else {
            return;
        };
        match Buffer::open(&path) {
            Ok(mut buffer) => {
                buffer.set_syntax(self.tabs[idx].syntax);
                self.tabs[idx].buffer = buffer;
                self.tabs[idx].editor = EditorState::default();
                self.tabs[idx].external_change = None;
                // `in-buffer-find-replace.md` §3.6: an external reload is
                // one of the explicit `FindBar::refresh` triggers, since the
                // matches an open bar is showing were computed against text
                // that no longer exists.
                if self.tabs[idx].find.is_open() {
                    let text = self.tabs[idx].buffer.text().to_string();
                    self.tabs[idx].find.refresh(&text, None);
                }
                self.refresh_blame_if_on(idx);
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    /// The doc §3.4 "Reload" action: re-reads the tab's file from disk
    /// through the normal `Buffer::open` path, replacing the buffer's
    /// text and clearing `external_change`. Discards unsaved edits and
    /// undo history, same as closing and reopening the tab would.
    fn reload_active_from_disk(&mut self) {
        if let Some(idx) = self.active_tab {
            self.reload_tab_from_disk(idx);
        }
    }

    /// The doc §3.4/§3.5 "Keep Mine" / dismiss action: clears
    /// `external_change` without touching the buffer. The next save still
    /// overwrites whatever is on disk, exactly as it already would.
    fn dismiss_external_change(&mut self) {
        if let Some(idx) = self.active_tab {
            self.tabs[idx].external_change = None;
        }
    }

    /// `⌘F`/`⌘R` shared logic (`in-buffer-find-replace.md` §3.1): seeds
    /// from the active selection when it's non-empty and single-line (a
    /// multi-line seed would make the query field wrap awkwardly and is
    /// almost never what's wanted), otherwise reuses whatever query this
    /// tab's bar already had. No-op if there is no active tab. Requests
    /// focus on the query field exactly on the transitions the doc names:
    /// the bar going from closed to open, or `with_replace` revealing the
    /// replace row on a bar that didn't have it showing yet.
    fn open_find_bar(&mut self, with_replace: bool) {
        let Some(idx) = self.active_tab else {
            return;
        };
        let was_open = self.tabs[idx].find.is_open();
        let was_replace_open = self.tabs[idx].find.replace_open();

        let tab = &self.tabs[idx];
        let selection = tab.buffer.text_buffer().selections().primary();
        let initial_query = (!selection.is_empty())
            .then(|| &tab.buffer.text()[selection.range()])
            .filter(|seed| !seed.contains('\n'))
            .map(str::to_string);
        let text = tab.buffer.text().to_string();

        self.tabs[idx].find.open(with_replace, initial_query, &text);
        if !was_open || (with_replace && !was_replace_open) {
            self.pending_find_focus = true;
        }
    }

    /// `⌘F`: opens the active tab's `FindBar` find-only, seeded from the
    /// active selection if non-empty (§3.1). No-op if there is no active
    /// tab.
    fn open_find(&mut self) {
        self.open_find_bar(false);
    }

    /// `⌘R`: same, with the replace row shown.
    fn open_replace(&mut self) {
        self.open_find_bar(true);
    }

    /// `Escape` while the bar owns focus, or its own close button (§3.6).
    fn close_find(&mut self) {
        if let Some(idx) = self.active_tab {
            self.tabs[idx].find.close();
        }
    }

    /// `⌘G`/`⌘⇧G`, and the "Replace" action's own advance-to-next-match
    /// step (§3.4, §3.8): moves the caret/selection to `range` and scrolls
    /// it into view via `EditorState::request_scroll` -- the same
    /// mechanism `CodeEditor::goto_offset` drives internally, generalised
    /// from a bare caret offset to a range, invoked directly here since
    /// this runs in `handle_shortcuts`/before the widget's own frame.
    fn goto_match(&mut self, idx: usize, range: Range<usize>) {
        self.tabs[idx]
            .buffer
            .text_buffer_mut()
            .set_selections(Selections::single(Selection::new(range.start, range.end)));
        self.tabs[idx].editor.request_scroll(range.start);
    }

    /// `⌘G`. No-op if the active tab's bar isn't open or has no matches.
    fn find_next(&mut self) {
        self.step_find(FindBar::next);
    }

    /// `⌘⇧G`.
    fn find_previous(&mut self) {
        self.step_find(FindBar::prev);
    }

    fn step_find(&mut self, step: impl FnOnce(&mut FindBar) -> Option<Range<usize>>) {
        let Some(idx) = self.active_tab else {
            return;
        };
        if !self.tabs[idx].find.is_open() {
            return;
        }
        let Some(range) = step(&mut self.tabs[idx].find) else {
            return;
        };
        self.goto_match(idx, range);
    }

    /// The replace-row "Replace" button / `⏎` in the replacement field
    /// (§3.8): replaces the current match through the buffer's normal
    /// transactional `apply` (one undo step, LSP notified like any other
    /// edit), then re-searches so `current` lands on the next match after
    /// where the replaced one used to start. No-op if there is no current
    /// match.
    fn replace_current_match(&mut self) {
        let Some(idx) = self.active_tab else {
            return;
        };
        let tab = &self.tabs[idx];
        let Some(current) = tab.find.current_match() else {
            return;
        };
        let Ok(query) = SearchQuery::compile(tab.find.query(), tab.find.options()) else {
            return;
        };
        let text = tab.buffer.text().to_string();
        let replacement = tab.find.replacement().to_string();
        let old_start = current.start;
        let transaction = replace_one(&text, &query, current, &replacement);

        self.tabs[idx].buffer.apply(transaction);
        self.notify_lsp_changed(idx);
        let new_text = self.tabs[idx].buffer.text().to_string();
        self.tabs[idx].find.refresh(&new_text, Some(old_start));
    }

    /// "Replace All" (§3.8): applies every match's replacement as one
    /// undo step, then re-searches. No-op if there are no matches. If the
    /// engine reports `truncated`, surfaces a one-line notice through
    /// `self.error` naming how many were replaced and that more remain
    /// (§2.1's "never silence" requirement, same channel
    /// `file-watcher.md` §3.6 uses for its own non-fatal degradation) --
    /// the bar stays open with its (now smaller) match list, so Replace
    /// All can simply be invoked again.
    fn replace_all_matches(&mut self) {
        let Some(idx) = self.active_tab else {
            return;
        };
        let tab = &self.tabs[idx];
        if tab.find.matches().is_empty() {
            return;
        }
        let Ok(query) = SearchQuery::compile(tab.find.query(), tab.find.options()) else {
            return;
        };
        let text = tab.buffer.text().to_string();
        let replacement = tab.find.replacement().to_string();
        let scope = tab.find.scope();
        let Some(ReplaceResult {
            transaction,
            truncated,
        }) = replace_all(&text, &query, &replacement, scope)
        else {
            return;
        };

        self.tabs[idx].buffer.apply(transaction);
        self.notify_lsp_changed(idx);
        let new_text = self.tabs[idx].buffer.text().to_string();
        self.tabs[idx].find.refresh(&new_text, None);
        if truncated {
            self.error = Some(format!(
                "Replaced {max} of {max}+ matches -- run Replace All again for the rest",
                max = ide_core::MAX_SEARCH_MATCHES,
            ));
        }
    }

    // ---- B3: command registry & palette ----

    /// `⌘⇧A`. Resets the query, selects the first row, and requests
    /// focus for the query field next frame (`command-palette.md` §3.1's
    /// dispatch loop is what makes this reachable even while the palette
    /// is already open -- it's the one binding still checked then).
    fn open_command_palette(&mut self) {
        self.command_palette_open = true;
        self.command_palette_query.clear();
        self.command_palette_selected = 0;
        self.pending_command_palette_focus = true;
    }

    /// `Escape` while the palette owns it, its own close control, or a
    /// successful `command_palette_confirm` (§3.4).
    fn close_command_palette(&mut self) {
        self.command_palette_open = false;
        self.command_palette_query.clear();
    }

    /// True exactly while the palette is open -- checked first in
    /// `handle_shortcuts`'s escape-arbitration chain (§3.5), ahead of the
    /// find bar and the Usages popup.
    fn command_palette_owns_escape(&self) -> bool {
        self.command_palette_open
    }

    /// `⇧⇧` / Go to File / Go to Class / Go to Symbol
    /// (`docs/features/search-everywhere.md` §3.2). `class_filter` is only
    /// meaningful when `tab == Symbols` (§3.4/§3.5).
    fn open_search_everywhere(&mut self, tab: SearchEverywhereTab, class_filter: bool) {
        self.search_everywhere_open = true;
        self.search_everywhere_tab = tab;
        self.search_everywhere_query.clear();
        self.search_everywhere_selected = 0;
        self.pending_search_everywhere_focus = true;
        self.search_everywhere_class_filter = class_filter;
    }

    /// `Escape`, the window's own close affordance, or a successful
    /// jump/run (§3.4). Discards any in-flight Files/Text background
    /// search, same as closing the Find in Path panel. Deliberately does
    /// **not** reset `last_workspace_symbol_query`/
    /// `document_symbols_requested_for` -- those are what makes the
    /// Symbols tab's known staleness (§4) self-correcting rather than
    /// reset-on-every-reopen, which would instead re-fire a request on
    /// every single reopen even when nothing changed.
    fn close_search_everywhere(&mut self) {
        self.search_everywhere_open = false;
        self.search_everywhere_files.discard_in_flight();
        self.search_everywhere_text.discard_in_flight();
        self.search_everywhere_class_filter = false;
    }

    /// True exactly while the popup is open -- checked in
    /// `handle_shortcuts`'s escape-arbitration chain alongside
    /// `command_palette_owns_escape` (§3.4).
    fn search_everywhere_owns_escape(&self) -> bool {
        self.search_everywhere_open
    }

    /// `⌘F12` (`file-structure-and-breadcrumbs.md` §2.3/§3.2). Only
    /// dispatched while `is_command_enabled(FileStructure)` -- an active
    /// tab with a real path -- so this itself doesn't re-check that.
    fn trigger_file_structure(&mut self) {
        self.file_structure_open = true;
        self.file_structure_query.clear();
        self.file_structure_selected = 0;
        self.pending_file_structure_focus = true;
    }

    fn close_file_structure(&mut self) {
        self.file_structure_open = false;
    }

    /// True exactly while the popup is open -- checked in
    /// `handle_shortcuts`'s escape-arbitration chain alongside
    /// `show_go_to_line` (§3.2): a small standalone modal, not part of the
    /// palette/Search Everywhere's own mutual-exclusion group.
    fn file_structure_owns_escape(&self) -> bool {
        self.file_structure_open
    }

    /// `↑`/`↓` while the popup is open (§3.2), wrapping -- same
    /// `rem_euclid` shape as `search_everywhere_move_selection`.
    fn file_structure_move_selection(&mut self, delta: isize) {
        let count = file_structure::visible_rows(
            self.active_document_symbols(),
            &self.file_structure_query,
        )
        .len();
        if count == 0 {
            return;
        }
        let current = self.file_structure_selected as isize;
        let next = (current + delta).rem_euclid(count as isize);
        self.file_structure_selected = next as usize;
    }

    /// `Enter`, or clicking a row (§3.2/§3.4): jumps to the selected row's
    /// symbol and closes the popup. No-op (dialog stays open) if the
    /// selection is out of range for the current row list.
    fn file_structure_confirm(&mut self) {
        let symbols = self.active_document_symbols();
        let rows = file_structure::visible_rows(symbols, &self.file_structure_query);
        let Some(row) = rows.get(self.file_structure_selected) else {
            return;
        };
        let symbol = symbols[row.symbol_index].clone();
        self.open_definition(&symbol.location.path, symbol.location.range.start);
        self.close_file_structure();
    }

    /// The active tab's own outline: `self.lsp.document_symbols` if it's
    /// currently answering for the active tab's own path, else `&[]` --
    /// same shape as `active_inlay_hints`/`active_semantic_tokens`.
    fn active_document_symbols(&self) -> &[Symbol] {
        let Some(idx) = self.active_tab else {
            return &[];
        };
        let Some(path) = self.tabs[idx].buffer.path() else {
            return &[];
        };
        if self.lsp.document_symbols_path.as_deref() != Some(path) {
            return &[];
        }
        &self.lsp.document_symbols
    }

    /// The chain of symbols the caret currently sits inside, outermost
    /// first -- feeds the breadcrumb bar (§2.3/§3.4). Like every other
    /// `active_cursor_offset`-driven read in this file (see
    /// `find_usages_target`'s own doc comment), this can trail one frame
    /// behind a just-happened tab switch or cursor move -- cosmetic only,
    /// self-corrects next frame.
    fn active_breadcrumbs(&self) -> Vec<&Symbol> {
        let symbols = self.active_document_symbols();
        if symbols.is_empty() {
            return Vec::new();
        }
        let Some(idx) = self.active_tab else {
            return Vec::new();
        };
        let Some(offset) = self.active_cursor_offset else {
            return Vec::new();
        };
        let text = self.tabs[idx].buffer.text();
        let Some(position) = ide_lsp::byte_offset_to_position(text, offset) else {
            return Vec::new();
        };
        ide_lsp::symbols_containing(symbols, position)
    }

    /// Called from the same two sites `sync_inlay_hints`/
    /// `sync_semantic_tokens` already fire from (§3.3): once on a tab's
    /// `DidOpen`, once on every actual (not idle-frame) `DidChange`.
    /// Always the whole document. Reuses `document_symbols_requested_for`
    /// rather than adding a second tracking field -- Search Everywhere's
    /// own lazy-fire in its Symbols-tab arm is left in place as a harmless
    /// fallback that already no-ops once this has landed for the same
    /// path.
    fn sync_document_symbols(&mut self, idx: usize) {
        let Some(path) = self.tabs[idx].buffer.path().map(Path::to_path_buf) else {
            return;
        };
        self.lsp.request_document_symbols(&path);
        self.document_symbols_requested_for = Some(path);
    }

    /// Cycles `search_everywhere_tab` by `delta` (`+1`/`-1`, wrapping)
    /// among the four variants in declaration order, resetting
    /// `search_everywhere_selected` to 0. Does not clear
    /// `search_everywhere_query` -- switching tabs keeps what was typed
    /// (§3.2).
    fn search_everywhere_switch_tab(&mut self, delta: isize) {
        const TABS: [SearchEverywhereTab; 4] = [
            SearchEverywhereTab::Files,
            SearchEverywhereTab::Symbols,
            SearchEverywhereTab::Actions,
            SearchEverywhereTab::Text,
        ];
        let current = TABS
            .iter()
            .position(|t| *t == self.search_everywhere_tab)
            .unwrap_or(0) as isize;
        let next = (current + delta).rem_euclid(TABS.len() as isize);
        self.search_everywhere_tab = TABS[next as usize];
        self.search_everywhere_selected = 0;
    }

    /// Called once per frame, right after `self.lsp.poll()`, alongside
    /// `sync_code_actions`/`handle_workspace_edit_ready` (§3.4). No-op
    /// unless `search_everywhere_open`. Drives whichever tab is active
    /// (§3.2/§3.3) -- `Files`/`Text` only actually launch a background
    /// search once the panel isn't already busy with an older query
    /// (`!panel.searching`), so a query that changes mid-search is picked
    /// up as soon as the in-flight one finishes rather than being dropped
    /// silently by `FilesSearchPanel::run`/`SearchPanel::run`'s own
    /// no-op-while-searching guard (§3.3's "never one-per-keystroke"
    /// throttling, made to still eventually reach the latest query).
    fn sync_search_everywhere(&mut self) {
        if !self.search_everywhere_open {
            return;
        }
        match self.search_everywhere_tab {
            SearchEverywhereTab::Files => {
                if self.search_everywhere_query.is_empty()
                    || self.search_everywhere_files.searching
                    || self.last_files_query.as_deref()
                        == Some(self.search_everywhere_query.as_str())
                {
                    return;
                }
                let Some(project) = &self.project else { return };
                let tree = project.scan_tree();
                self.search_everywhere_files.discard_in_flight();
                self.search_everywhere_files
                    .run(tree, self.search_everywhere_query.clone());
                self.last_files_query = Some(self.search_everywhere_query.clone());
            }
            SearchEverywhereTab::Text => {
                if self.search_everywhere_query.is_empty()
                    || self.search_everywhere_text.searching
                    || self.last_text_query.as_deref()
                        == Some(self.search_everywhere_query.as_str())
                {
                    return;
                }
                let Some(project) = &self.project else { return };
                let tree = project.scan_tree();
                self.search_everywhere_text.discard_in_flight();
                self.search_everywhere_text
                    .run(tree, self.search_everywhere_query.clone());
                self.last_text_query = Some(self.search_everywhere_query.clone());
            }
            SearchEverywhereTab::Symbols => {
                if self.search_everywhere_query.is_empty() {
                    if let Some(idx) = self.active_tab {
                        if let Some(path) = self.tabs[idx].buffer.path().map(Path::to_path_buf) {
                            if self.document_symbols_requested_for.as_deref()
                                != Some(path.as_path())
                            {
                                self.lsp.request_document_symbols(&path);
                                self.document_symbols_requested_for = Some(path);
                            }
                        }
                    }
                } else if self.last_workspace_symbol_query.as_deref()
                    != Some(self.search_everywhere_query.as_str())
                {
                    self.lsp
                        .query_workspace_symbols(&self.search_everywhere_query);
                    self.last_workspace_symbol_query = Some(self.search_everywhere_query.clone());
                }
            }
            SearchEverywhereTab::Actions => {}
        }
    }

    /// Every result the active tab currently has to show (§3.2).
    fn search_everywhere_rows(&self) -> Vec<SearchEverywhereRow> {
        match self.search_everywhere_tab {
            SearchEverywhereTab::Files => self
                .search_everywhere_files
                .results
                .as_ref()
                .map(|r| {
                    r.matches
                        .iter()
                        .cloned()
                        .map(SearchEverywhereRow::File)
                        .collect()
                })
                .unwrap_or_default(),
            SearchEverywhereTab::Symbols => {
                let symbols: &[Symbol] = if self.search_everywhere_query.is_empty() {
                    &self.lsp.document_symbols
                } else {
                    &self.lsp.workspace_symbols
                };
                symbols
                    .iter()
                    .filter(|s| {
                        !self.search_everywhere_class_filter || is_class_like_symbol(s.kind)
                    })
                    .cloned()
                    .map(SearchEverywhereRow::Symbol)
                    .collect()
            }
            SearchEverywhereTab::Actions => {
                let query = self.search_everywhere_query.trim();
                let mut scored: Vec<(i64, &'static command::Command)> = command::commands()
                    .iter()
                    .filter_map(|cmd| {
                        let haystack = format!("{} {}", cmd.title, cmd.category);
                        ide_core::fuzzy_score(query, &haystack).map(|m| (m.score, cmd))
                    })
                    .collect();
                scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
                scored
                    .into_iter()
                    .map(|(_, cmd)| SearchEverywhereRow::Action(cmd))
                    .collect()
            }
            SearchEverywhereTab::Text => self
                .search_everywhere_text
                .results
                .as_ref()
                .map(|r| {
                    r.matches
                        .iter()
                        .cloned()
                        .map(SearchEverywhereRow::Text)
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    /// Moves `search_everywhere_selected` by `delta`, wrapping, no-op on
    /// an empty row list -- same shape as `command_palette_move_selection`.
    fn search_everywhere_move_selection(&mut self, delta: isize) {
        let count = self.search_everywhere_rows().len();
        if count == 0 {
            return;
        }
        let current = self.search_everywhere_selected as isize;
        let next = (current + delta).rem_euclid(count as isize);
        self.search_everywhere_selected = next as usize;
    }

    /// `Enter`, or clicking a row (§3.2): dispatches on the selected
    /// `SearchEverywhereRow`. Every successful dispatch closes the popup;
    /// a disabled `Action` row is the only no-op (popup stays open, same
    /// as the command palette's own disabled-row behavior).
    fn search_everywhere_confirm(&mut self, ctx: &egui::Context) {
        let rows = self.search_everywhere_rows();
        let Some(row) = rows.into_iter().nth(self.search_everywhere_selected) else {
            return;
        };
        match row {
            SearchEverywhereRow::File(m) => {
                self.open_file(&m.path);
                self.push_nav_location();
                self.close_search_everywhere();
            }
            SearchEverywhereRow::Text(m) => {
                self.open_search_result(&m.path, m.byte_offset);
                self.close_search_everywhere();
            }
            SearchEverywhereRow::Symbol(s) => {
                self.open_definition(&s.location.path, s.location.range.start);
                self.close_search_everywhere();
            }
            SearchEverywhereRow::Action(cmd) => {
                if !self.is_command_enabled(cmd.action) {
                    return;
                }
                self.run_command(cmd.action, ctx);
                self.close_search_everywhere();
            }
        }
    }

    /// `⌘L` (`Ctrl+G`) entry point (§3.5): opens the small Go to Line
    /// dialog, resetting its input and requesting focus next frame.
    fn trigger_go_to_line(&mut self) {
        self.show_go_to_line = true;
        self.go_to_line_input.clear();
        self.pending_go_to_line_focus = true;
    }

    /// `render_go_to_line_dialog`'s Enter/OK handler (§3.6): parses
    /// `go_to_line_input` as `"<line>"` or `"<line>:<column>"`, both
    /// 1-based. Anything that doesn't parse as a positive `u32` (empty,
    /// non-numeric, `0`, a bare `:`) is a silent no-op, dialog stays open.
    /// A `line` past the buffer's actual line count clamps to the last
    /// line; a `column` past that line's length clamps to the line's end.
    fn confirm_go_to_line(&mut self) {
        let Some(idx) = self.active_tab else { return };
        let input = self.go_to_line_input.trim();
        let (line_part, column_part) = match input.split_once(':') {
            Some((l, c)) => (l, Some(c)),
            None => (input, None),
        };
        let Ok(line) = line_part.trim().parse::<u32>() else {
            return;
        };
        if line == 0 {
            return;
        }
        let column = match column_part {
            Some(c) => {
                let Ok(column) = c.trim().parse::<u32>() else {
                    return;
                };
                if column == 0 {
                    return;
                }
                column
            }
            None => 1,
        };

        let text = self.tabs[idx].buffer.text();
        let lines: Vec<&str> = text.split('\n').collect();
        let line_idx = (line as usize - 1).min(lines.len().saturating_sub(1));
        let line_text = lines[line_idx];
        let col_idx = (column as usize - 1).min(line_text.chars().count());
        let byte_in_line: usize = line_text.chars().take(col_idx).map(char::len_utf8).sum();
        let line_start: usize = lines[..line_idx].iter().map(|l| l.len() + 1).sum();

        self.pending_cursor_offset = Some(line_start + byte_in_line);
        self.push_nav_location();
        self.show_go_to_line = false;
    }

    /// Every registered command whose `title` or `category` contains
    /// `command_palette_query`, case-insensitively, substring match, in
    /// `command::commands()`'s declaration order (§4.2 -- not a fuzzy
    /// matcher; that's **C2**'s job).
    fn filtered_commands(&self) -> Vec<&'static command::Command> {
        let query = self.command_palette_query.to_lowercase();
        command::commands()
            .iter()
            .filter(|c| {
                query.is_empty()
                    || c.title.to_lowercase().contains(&query)
                    || c.category.to_lowercase().contains(&query)
            })
            .collect()
    }

    /// Moves `command_palette_selected` by `delta`, wrapping at both ends
    /// of `filtered_commands()`. No-op on an empty filtered list.
    fn command_palette_move_selection(&mut self, delta: isize) {
        let count = self.filtered_commands().len();
        if count == 0 {
            return;
        }
        let current = self.command_palette_selected as isize;
        let next = (current + delta).rem_euclid(count as isize);
        self.command_palette_selected = next as usize;
    }

    /// `Enter`, or clicking a row: if the currently-selected filtered
    /// command is enabled, runs it and closes the palette -- except
    /// `FindAction`, whose own effect *is* "the palette is open, freshly
    /// reset" (`open_command_palette`), so closing immediately afterward
    /// would undo what it just did (§3.3). If the row is disabled, does
    /// nothing and leaves the palette open (§3.4).
    fn command_palette_confirm(&mut self, ctx: &egui::Context) {
        let Some(cmd) = self
            .filtered_commands()
            .get(self.command_palette_selected)
            .copied()
        else {
            return;
        };
        if !self.is_command_enabled(cmd.action) {
            return;
        }
        let action = cmd.action;
        self.run_command(action, ctx);
        if action != CommandAction::FindAction {
            self.close_command_palette();
        }
    }

    /// Whether `action` can actually do something right now. Read by
    /// both the palette (to gray out a row) and `handle_shortcuts` (to
    /// decide whether a live keypress actually dispatches, §3.1).
    fn is_command_enabled(&self, action: CommandAction) -> bool {
        match action {
            CommandAction::FindInPath => self.project.is_some(),
            CommandAction::ReplaceInPath => self.project.is_some(),
            CommandAction::FindAction => true,
            CommandAction::SaveAll
            | CommandAction::Undo
            | CommandAction::Redo
            | CommandAction::FindUsages
            | CommandAction::ShowUsages
            | CommandAction::Find
            | CommandAction::Replace
            | CommandAction::ReplaceAll
            | CommandAction::FindNext
            | CommandAction::FindPrevious
            | CommandAction::CollapseFold
            | CommandAction::ExpandFold
            | CommandAction::CollapseAllFolds
            | CommandAction::ExpandAllFolds
            | CommandAction::GoToDeclaration
            | CommandAction::GoToImplementation
            | CommandAction::GoToTypeDeclaration
            | CommandAction::QuickDocumentation
            | CommandAction::ShowIntentionActions => self.active_tab.is_some(),
            CommandAction::NavigateBack => self.nav.can_go_back(),
            CommandAction::NavigateForward => self.nav.can_go_forward(),
            CommandAction::ToggleTheme
            | CommandAction::ToggleProjectToolWindow
            | CommandAction::ToggleFindToolWindow
            | CommandAction::ToggleRunToolWindow
            | CommandAction::ToggleProblemsToolWindow
            | CommandAction::ToggleClaudeToolWindow
            | CommandAction::ToggleZenMode
            | CommandAction::ShowLanguageSettings
            | CommandAction::ShowKeymapSettings
            | CommandAction::RecentFiles
            | CommandAction::RecentLocations => true,
            CommandAction::RefreshTree | CommandAction::ToggleVcsToolWindow => {
                self.project.is_some()
            }
            CommandAction::ToggleSmartMode => !self.active_languages.is_empty(),
            CommandAction::RunCargo(_) => self.is_rust_project(),
            CommandAction::GoToFile | CommandAction::GoToClass | CommandAction::GoToSymbol => {
                self.project.is_some()
            }
            CommandAction::GoToLine => {
                self.active_tab.is_some() && self.view_mode == ViewMode::Editor
            }
            CommandAction::ReformatCode | CommandAction::FileStructure => self
                .active_tab
                .is_some_and(|idx| self.tabs[idx].buffer.path().is_some()),
            CommandAction::ToggleFormatOnSave => true,
            // Unlike `ReformatCode`, there is no self-resolving fallback
            // that makes triggering Rename meaningful without a server, so
            // the palette greys it out instead of opening a popup that can
            // only ever fail (`docs/features/rename-refactoring.md` §2.3).
            CommandAction::Rename
            | CommandAction::RefactorThis
            | CommandAction::ExtractVariable
            | CommandAction::ExtractMethod
            | CommandAction::ExtractConstant
            | CommandAction::ExtractField
            | CommandAction::Inline
            | CommandAction::GenerateMenu
            | CommandAction::ImplementMethods
            | CommandAction::OverrideMethods
            | CommandAction::CreateTest
            | CommandAction::OptimizeImports => self.active_tab.is_some_and(|idx| {
                self.tabs[idx]
                    .buffer
                    .path()
                    .is_some_and(|path| self.lsp.is_running_for(path))
            }),
            CommandAction::NextTab | CommandAction::PreviousTab | CommandAction::CloseTab => {
                self.active_tab.is_some()
            }
            CommandAction::GitBranches => self.project.is_some(),
            CommandAction::ToggleBlameAnnotations => self
                .active_tab
                .is_some_and(|idx| self.tabs[idx].buffer.path().is_some()),
        }
    }

    /// The dispatch table: matches `action` to the existing per-action
    /// method. Does not itself check `is_command_enabled` -- every call
    /// site (`handle_shortcuts`, `command_palette_confirm`) checks first,
    /// same as each of these methods already no-ops safely on its own
    /// preconditions. Takes `ctx` only for `ToggleTheme`, which needs it to
    /// re-apply `egui::Visuals` immediately (`toggle_theme`'s existing
    /// signature) -- every other action already gets everything it needs
    /// from `self`.
    fn run_command(&mut self, action: CommandAction, ctx: &egui::Context) {
        match action {
            CommandAction::SaveAll => self.try_save_active(),
            CommandAction::Undo => self.undo_active(),
            CommandAction::Redo => self.redo_active(),
            CommandAction::FindUsages => self.trigger_find_usages(),
            CommandAction::ShowUsages => self.trigger_find_usages_popup(),
            CommandAction::FindInPath => self.trigger_search(),
            CommandAction::Find => self.open_find(),
            CommandAction::Replace => self.open_replace(),
            CommandAction::ReplaceAll => self.replace_all_matches(),
            CommandAction::ReplaceInPath => self.trigger_replace_in_path(),
            CommandAction::FindNext => self.find_next(),
            CommandAction::FindPrevious => self.find_previous(),
            CommandAction::FindAction => self.open_command_palette(),
            CommandAction::ToggleTheme => self.toggle_theme(ctx),
            CommandAction::RefreshTree => self.refresh_tree(),
            CommandAction::ToggleSmartMode => self.toggle_smart_mode(),
            CommandAction::RunCargo(command) => self.run_cargo(command),
            CommandAction::ToggleProjectToolWindow => self.toggle_tool_window(ToolWindow::Project),
            CommandAction::ToggleFindToolWindow => {
                self.toggle_bottom_tool_window(BottomView::Search)
            }
            CommandAction::ToggleRunToolWindow => {
                self.toggle_bottom_tool_window(BottomView::CargoOutput)
            }
            CommandAction::ToggleProblemsToolWindow => {
                self.toggle_bottom_tool_window(BottomView::Problems)
            }
            CommandAction::ToggleVcsToolWindow => self.toggle_view_mode(),
            CommandAction::ToggleClaudeToolWindow => self.toggle_tool_window(ToolWindow::Claude),
            CommandAction::ToggleZenMode => self.toggle_zen_mode(),
            CommandAction::ShowLanguageSettings => self.show_language_settings = true,
            CommandAction::ShowKeymapSettings => self.show_keymap_settings = true,
            CommandAction::CollapseFold => self.collapse_fold_at_caret(),
            CommandAction::ExpandFold => self.expand_fold_at_caret(),
            CommandAction::CollapseAllFolds => self.collapse_all_folds(),
            CommandAction::ExpandAllFolds => self.expand_all_folds(),
            CommandAction::GoToDeclaration => self.trigger_go_to_declaration(),
            CommandAction::GoToImplementation => self.trigger_go_to_implementation(),
            CommandAction::GoToTypeDeclaration => self.trigger_go_to_type_declaration(),
            CommandAction::NavigateBack => self.nav_back(),
            CommandAction::NavigateForward => self.nav_forward(),
            CommandAction::QuickDocumentation => self.trigger_quick_documentation(),
            CommandAction::ShowIntentionActions => self.trigger_show_intention_actions(),
            CommandAction::GoToFile => {
                self.open_search_everywhere(SearchEverywhereTab::Files, false)
            }
            CommandAction::GoToClass => {
                self.open_search_everywhere(SearchEverywhereTab::Symbols, true)
            }
            CommandAction::GoToSymbol => {
                self.open_search_everywhere(SearchEverywhereTab::Symbols, false)
            }
            CommandAction::GoToLine => self.trigger_go_to_line(),
            CommandAction::FileStructure => self.trigger_file_structure(),
            CommandAction::RecentFiles => self.trigger_recent_files(),
            CommandAction::RecentLocations => self.trigger_recent_locations(),
            CommandAction::ReformatCode => self.trigger_reformat_code(),
            CommandAction::ToggleFormatOnSave => self.format_on_save = !self.format_on_save,
            CommandAction::Rename => self.trigger_rename(),
            CommandAction::RefactorThis => self.trigger_refactor_this(),
            CommandAction::ExtractVariable => {
                self.trigger_direct_refactor(DirectRefactorKind::ExtractVariable)
            }
            CommandAction::ExtractMethod => {
                self.trigger_direct_refactor(DirectRefactorKind::ExtractMethod)
            }
            CommandAction::ExtractConstant => {
                self.trigger_direct_refactor(DirectRefactorKind::ExtractConstant)
            }
            CommandAction::ExtractField => {
                self.trigger_direct_refactor(DirectRefactorKind::ExtractField)
            }
            CommandAction::Inline => self.trigger_direct_refactor(DirectRefactorKind::Inline),
            CommandAction::GenerateMenu => self.trigger_generate_menu(),
            CommandAction::ImplementMethods => {
                self.trigger_direct_generate(DirectGenerateKind::ImplementMethods)
            }
            CommandAction::OverrideMethods => {
                self.trigger_direct_generate(DirectGenerateKind::OverrideMethods)
            }
            CommandAction::CreateTest => {
                self.trigger_direct_generate(DirectGenerateKind::CreateTest)
            }
            CommandAction::OptimizeImports => self.trigger_optimize_imports(),
            CommandAction::NextTab => self.cycle_tab(1),
            CommandAction::PreviousTab => self.cycle_tab(-1),
            CommandAction::CloseTab => {
                if let Some(idx) = self.active_tab {
                    self.request_close_tab(idx);
                }
            }
            CommandAction::GitBranches => {
                if let Some(root) = self.project.as_ref().map(|p| p.root().to_path_buf()) {
                    self.git.open_branches_popup(&root);
                }
            }
            CommandAction::ToggleBlameAnnotations => self.toggle_blame_annotations(),
        }
    }

    /// `CollapseFold` (`code-folding.md` §2.4/§2.5): collapses the
    /// innermost fold range containing the caret's line, then re-lands the
    /// caret on that range's `start_line` if collapsing just hid it (§2.6).
    fn collapse_fold_at_caret(&mut self) {
        let Some(idx) = self.active_tab else { return };
        let Some(offset) = self.active_cursor_offset else {
            return;
        };
        let ranges = self.tabs[idx].buffer.text_buffer().fold_ranges();
        let (line, _) = editor::cursor_line_column(self.tabs[idx].buffer.text_buffer(), offset);
        let tab = &mut self.tabs[idx];
        tab.editor.collapse_at_caret(&ranges, line);
        editor::folding::reveal_caret_after_collapse(&mut tab.buffer, &tab.editor);
    }

    /// `ExpandFold`: uncollapses the range whose `start_line` is the
    /// caret's line, if one is collapsed there. No caret redirect needed --
    /// expanding never hides the caret's own line.
    fn expand_fold_at_caret(&mut self) {
        let Some(idx) = self.active_tab else { return };
        let Some(offset) = self.active_cursor_offset else {
            return;
        };
        let (line, _) = editor::cursor_line_column(self.tabs[idx].buffer.text_buffer(), offset);
        self.tabs[idx].editor.expand_at_caret(line);
    }

    /// `CollapseAllFolds`: collapses every fold range in the active tab,
    /// then reveals every caret hidden by the result -- a multi-cursor
    /// buffer can have more than one caret hidden at once (§2.6).
    fn collapse_all_folds(&mut self) {
        let Some(idx) = self.active_tab else { return };
        let ranges = self.tabs[idx].buffer.text_buffer().fold_ranges();
        let tab = &mut self.tabs[idx];
        tab.editor.collapse_all(&ranges);
        editor::folding::reveal_caret_after_collapse(&mut tab.buffer, &tab.editor);
    }

    /// `ExpandAllFolds`: uncollapses every fold range in the active tab.
    fn expand_all_folds(&mut self) {
        let Some(idx) = self.active_tab else { return };
        self.tabs[idx].editor.expand_all();
    }

    /// Ids of `command::commands()` whose title, category, or effective
    /// binding's label (mac and non-mac form) contains `keymap_search`
    /// case-insensitively, in registry order. Empty query returns every
    /// id -- serves both "search by action" and "search by pressed
    /// combination" (`keymap.md` §3.5) since the label text itself is
    /// what gets matched.
    fn keymap_filtered_ids(&self) -> Vec<&'static str> {
        let query = self.keymap_search.to_lowercase();
        command::commands()
            .iter()
            .filter(|c| {
                if query.is_empty() {
                    return true;
                }
                let label_match = self.keymap.effective_binding(c.id).is_some_and(|b| {
                    let chord = b.for_platform();
                    chord.label(true).to_lowercase().contains(&query)
                        || chord.label(false).to_lowercase().contains(&query)
                });
                c.title.to_lowercase().contains(&query)
                    || c.category.to_lowercase().contains(&query)
                    || label_match
            })
            .map(|c| c.id)
            .collect()
    }

    /// Starts capture mode for `id` (`keymap.md` §2.6): clears any stale
    /// pending capture from a previous row's edit.
    fn start_keymap_capture(&mut self, id: &'static str) {
        self.keymap_capture_target = Some(id);
        self.keymap_capture_pending = None;
    }

    /// Called once per frame while `keymap_capture_target.is_some()`: on
    /// the first non-modifier key event this frame, builds a `KeyChord`
    /// and stores it plus its conflicts in `keymap_capture_pending`,
    /// awaiting `confirm_keymap_capture`/`cancel_keymap_capture`.
    /// `keymap_capture_target` stays set across this -- unlike `keymap.md`
    /// §2.6's original description, clearing it here would lose which
    /// command the pending capture is *for*, since `keymap_capture_pending`
    /// itself carries no id (found while implementing; both fields now
    /// stay set together until Confirm/Cancel clears both).
    fn poll_keymap_capture(&mut self, ctx: &egui::Context) {
        let Some(target) = self.keymap_capture_target else {
            return;
        };
        if self.keymap_capture_pending.is_some() {
            return;
        }
        let chord = ctx.input(|i| {
            i.events.iter().find_map(|event| match event {
                egui::Event::Key {
                    key, pressed: true, ..
                } if !is_bare_modifier_key(*key) => Some(KeyChord {
                    key: *key,
                    modifiers: i.modifiers,
                }),
                _ => None,
            })
        });
        if let Some(chord) = chord {
            let conflicts = self.keymap.conflicts(target, chord);
            self.keymap_capture_pending = Some((chord, conflicts));
        }
    }

    /// Commits `keymap_capture_pending`'s chord via `set_override`, clears
    /// both capture fields. No-op if nothing is pending.
    fn confirm_keymap_capture(&mut self) {
        let Some(target) = self.keymap_capture_target else {
            return;
        };
        let Some((chord, _)) = self.keymap_capture_pending.take() else {
            return;
        };
        self.keymap.set_override(target, Some(Binding::same(chord)));
        self.keymap_capture_target = None;
    }

    /// Clears both capture fields without committing anything.
    fn cancel_keymap_capture(&mut self) {
        self.keymap_capture_target = None;
        self.keymap_capture_pending = None;
    }

    fn reset_keymap_binding(&mut self, id: &str) {
        self.keymap.reset(id);
    }

    /// `keymap.md` §2.6's `export_keymap`/`import_keymap` are split here
    /// into a path-taking core operation (this method, directly testable)
    /// plus a render.rs button handler that owns the `rfd::FileDialog`
    /// call -- the same split `save_active_as`/its render.rs "Save As…"
    /// button already use, found necessary while implementing since a
    /// method that opens a real OS file dialog itself can't be exercised
    /// by a normal `cargo test` run.
    fn export_keymap_to(&self, path: &Path) -> std::io::Result<()> {
        std::fs::write(path, self.keymap.export())
    }

    /// Reads and parses `path` via `KeymapOverlay::import`, which is
    /// atomic on failure (`keymap.md` §2.5) -- `self.keymap` is left
    /// untouched if this returns `Err`.
    fn import_keymap_from(&mut self, path: &Path) -> Result<ImportReport, String> {
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        self.keymap.import(&text).map_err(|e| e.to_string())
    }
}

/// Status bar's "Encoding" field (§3.6), defaulting to `"UTF-8"` when no
/// `.editorconfig` charset is set -- the buffer's actual behaviour
/// (`buffer.rs`'s `save_with`, per `editorconfig::save_charset`).
fn charset_label(charset: Option<Charset>) -> &'static str {
    match charset {
        None | Some(Charset::Utf8) => "UTF-8",
        Some(Charset::Utf8Bom) => "UTF-8 BOM",
        Some(Charset::Latin1) => "Latin-1",
        Some(Charset::Utf16Le) => "UTF-16LE",
        Some(Charset::Utf16Be) => "UTF-16BE",
    }
}

/// Status bar's "Line ending" field, defaulting to `"LF"`.
fn end_of_line_label(end_of_line: Option<EndOfLine>) -> &'static str {
    match end_of_line {
        None | Some(EndOfLine::Lf) => "LF",
        Some(EndOfLine::Crlf) => "CRLF",
        Some(EndOfLine::Cr) => "CR",
    }
}

/// Converts a `codeAction/resolve`d (or already-present) LSP `WorkspaceEdit`
/// entry's position-addressed `TextEdit`s into an `ide_core::text::
/// Transaction` against `text`'s *current* content
/// (`docs/features/code-actions.md` §3.4, §4). `None` if any edit's
/// position doesn't convert to a valid byte offset in `text`, or if the
/// converted ranges overlap (a malicious or buggy server response) --
/// either way the whole file's edit is rejected, mirroring `ide-lsp`'s own
/// `convert_workspace_edit` fail-the-whole-batch rule one level down.
///
/// Builds a single `LineIndex` up front and resolves every `TextEdit`'s
/// `Position`s through it, rather than calling `ide_lsp::position_to_byte_
/// offset` once per edit -- that function re-scans `text` from the start
/// on every call (`crates/lsp/src/position.rs`), so calling it N times for
/// a `WorkspaceEdit` with N edits spread across an N-line file (a
/// project-wide rename or reformat's natural shape) is O(N²) and can hang
/// the UI thread for minutes, since this whole function runs synchronously
/// on it (`docs/security-findings/rust-ui-dev-code-actions-2026-08-20.md`
/// finding 1). `LineIndex::new` is a single O(text length) pass, and
/// `line_start` is O(1) after that, making this whole conversion O(text
/// length + edit count).
fn workspace_text_edits_to_transaction(
    text: &str,
    text_edits: &[ide_lsp::TextEdit],
) -> Option<ide_core::text::Transaction> {
    let line_index = ide_core::text::LineIndex::new(text);
    let mut changes = Vec::with_capacity(text_edits.len());
    for edit in text_edits {
        let start = position_to_byte_offset_indexed(&line_index, text, edit.range.start)?;
        let end = position_to_byte_offset_indexed(&line_index, text, edit.range.end)?;
        changes.push(ide_core::text::Change::new(
            start..end,
            edit.new_text.clone(),
        ));
    }
    ide_core::text::Transaction::new(changes).ok()
}

/// Parses the Search panel's comma-separated Include/Exclude text fields
/// into the glob list `ide_core::PathSearchOptions` actually wants
/// (`search-in-path-v2.md` §2.2) -- trims whitespace around each entry and
/// drops empties, so `"*.rs, , *.toml"` and `"*.rs,*.toml"` parse
/// identically. Called only at submit time (`IdeApp::sync_search_glob_
/// options`), never per-frame from `render_search_panel` -- see
/// `search_include_text`'s doc comment for why re-deriving the text field
/// from the parsed `Vec<String>` every frame is wrong (rev finding, fix
/// round 1).
fn split_glob_list(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Same LF-line-ending assumption, UTF-16-column semantics, and
/// out-of-range/surrogate-pair rejection as `ide_lsp::position_to_byte_
/// offset` (`crates/lsp/src/position.rs`), with one deliberate
/// improvement rather than a bug-for-bug port: `position.line ==
/// line_index.line_count()` (exactly one line past the document's last
/// real line, `character: 0`) is a common LSP encoding for "insert/delete
/// at end of file", and is accepted here as the empty line at true EOF.
/// `ide_lsp::position_to_byte_offset` also accepts this position, but
/// returns `text.len() + 1` for it -- one byte **past** the end of the
/// string, confirmed live for every text tried (with or without a
/// trailing newline) -- because its `text.split('\n')`-consuming loop
/// unconditionally adds `line.len() + 1` for a final segment that may not
/// actually end in a `'\n'`. That out-of-bounds offset happens to be
/// tolerated downstream today (`TextBuffer::apply` clamps it; `ide_core::
/// apply_workspace_edit_to_disk`'s stricter check rejects it as
/// `OffsetOutOfRange`), but reproducing it here rather than the correct
/// `text.len()` would still be reproducing a bug, not "no behavioral
/// change" -- this is `ide-ui`-local, so the fix is made here rather than
/// also touching `crates/lsp/**` (out of this diff's scope; worth a
/// follow-up there).
fn position_to_byte_offset_indexed(
    line_index: &ide_core::text::LineIndex,
    text: &str,
    position: ide_lsp::Position,
) -> Option<usize> {
    let line = position.line as usize;
    let (line_start, line_end) = if line == line_index.line_count() {
        (text.len(), text.len())
    } else {
        let line_start = line_index.line_start(line)?;
        let line_end = match line_index.line_start(line + 1) {
            Some(next_start) => next_start - 1,
            None => text.len(),
        };
        (line_start, line_end)
    };
    let line_text = &text[line_start..line_end];

    let mut utf16_count = 0u32;
    for (idx, ch) in line_text.char_indices() {
        if utf16_count == position.character {
            return Some(line_start + idx);
        }
        utf16_count += ch.len_utf16() as u32;
        if utf16_count > position.character {
            return None;
        }
    }
    if utf16_count == position.character {
        Some(line_start + line_text.len())
    } else {
        None
    }
}

/// Status bar's "Indent" field, e.g. `"Spaces: 4"` -- takes the tab's
/// already-resolved `IndentUnit` (`EditorState::indent`, itself derived
/// from `.editorconfig` with `IndentUnit::default()`'s own fallback for
/// whatever property wasn't set) rather than re-deriving it from
/// `EditorConfig`, so there is exactly one place that resolution happens.
fn indent_label(unit: IndentUnit) -> String {
    match unit.style {
        IndentStyle::Spaces => format!("Spaces: {}", unit.width),
        IndentStyle::Tabs => format!("Tabs: {}", unit.width),
    }
}

/// `egui::Key`'s eight physical left/right modifier variants -- excluded
/// from `poll_keymap_capture` so pressing bare `⌘` while starting to hold
/// a chord can't itself be captured as the chord's key (`keymap.md` §2.6).
fn is_bare_modifier_key(key: egui::Key) -> bool {
    matches!(
        key,
        egui::Key::ShiftLeft
            | egui::Key::ShiftRight
            | egui::Key::ControlLeft
            | egui::Key::ControlRight
            | egui::Key::AltLeft
            | egui::Key::AltRight
            | egui::Key::SuperLeft
            | egui::Key::SuperRight
    )
}

/// Go to Class's Symbols-tab filter (§3.5): the "class-like" `SymbolKind`s.
fn is_class_like_symbol(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class | SymbolKind::Struct | SymbolKind::Interface | SymbolKind::Enum
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ide_core::Token;
    use ide_lsp::CodeAction;

    // ---- Tab / view mode ----

    #[test]
    fn view_mode_toggle_flips() {
        assert_eq!(ViewMode::Editor.toggled(), ViewMode::SourceControl);
        assert_eq!(ViewMode::SourceControl.toggled(), ViewMode::Editor);
    }

    fn app_without_gui() -> IdeApp {
        IdeApp {
            active_cursor_offset: None,
            theme: Theme::Dark,
            view_mode: ViewMode::Editor,
            bottom_view: BottomView::Problems,
            project: None,
            tree: None,
            tree_scan: TreeScan::default(),
            pending_tree_scan_kind: None,
            tabs: Vec::new(),
            active_tab: None,
            untitled_count: 0,
            pending_confirm: None,
            should_quit: false,
            error: None,
            pending_create_parent: None,
            create_project_name: String::new(),
            claude: ClaudePanel::default(),
            claude_terminals: ClaudeTerminalPanel::default(),
            claude_view: ClaudeView::Chat,
            git: GitPanel::default(),
            lsp: LspBridge::default(),
            cargo: CargoPanel::default(),
            clone: CloneState::default(),
            watcher: None,
            pending_cursor_offset: None,
            hover_link: None,
            show_usages_popup: false,
            goto_action: None,
            show_goto_popup: false,
            goto_declaration_origin: None,
            pending_interface_check: None,
            show_hover_popup: false,
            last_highlighted_target: None,
            show_code_actions_popup: false,
            last_code_actions_target: None,
            show_refactor_menu_popup: false,
            show_generate_menu_popup: false,
            recent_files: Vec::new(),
            recent_files_open: false,
            recent_files_query: String::new(),
            recent_files_selected: 0,
            pending_recent_files_focus: false,
            recent_locations_open: false,
            recent_locations_selected: 0,
            via_refactor_preview: false,
            pending_refactor_preview: None,
            git_gutter: Vec::new(),
            git_gutter_path: None,
            git_gutter_popup_line: None,
            blame_popup_commit_id: None,
            pending_find_focus: false,
            search: PathSearchPanel::default(),
            search_query: String::new(),
            search_options: PathSearchOptions {
                search: ide_core::buffer_search::SearchOptions::default(),
                include: Vec::new(),
                exclude: Vec::new(),
                respect_gitignore: true,
            },
            search_include_text: String::new(),
            search_exclude_text: String::new(),
            search_replacement: String::new(),
            search_replace_open: false,
            pending_replace_in_path_preview: None,
            custom_languages: Vec::new(),
            active_languages: Vec::new(),
            new_language_name: String::new(),
            new_language_extension: String::new(),
            new_language_command: String::new(),
            new_language_args: String::new(),
            language_settings_error: None,
            show_language_settings: false,
            dismissed_language_suggestions: Vec::new(),
            pending_language_suggestions: Vec::new(),
            command_palette_open: false,
            command_palette_query: String::new(),
            command_palette_selected: 0,
            pending_command_palette_focus: false,
            keymap: KeymapOverlay::default(),
            show_keymap_settings: false,
            keymap_search: String::new(),
            keymap_capture_target: None,
            keymap_capture_pending: None,
            keymap_import_error: None,
            nav: NavHistory::default(),
            show_project_tool_window: true,
            show_claude_tool_window: true,
            show_bottom_tool_window: true,
            zen_mode: false,
            search_everywhere_open: false,
            search_everywhere_tab: SearchEverywhereTab::Files,
            search_everywhere_query: String::new(),
            search_everywhere_selected: 0,
            pending_search_everywhere_focus: false,
            search_everywhere_class_filter: false,
            last_workspace_symbol_query: None,
            document_symbols_requested_for: None,
            file_structure_open: false,
            file_structure_query: String::new(),
            file_structure_selected: 0,
            pending_file_structure_focus: false,
            last_files_query: None,
            last_text_query: None,
            search_everywhere_text: SearchPanel::default(),
            search_everywhere_files: files_search::FilesSearchPanel::default(),
            show_go_to_line: false,
            go_to_line_input: String::new(),
            pending_go_to_line_focus: false,
            search_everywhere_double_tap: editor::double_tap::DoubleTap::default(),
            search_everywhere_shift_down: false,
            format_on_save: false,
            format_on_save_target: None,
            rename_popup: None,
            pending_rename_focus: false,
            pending_rename_preview: None,
        }
    }

    #[test]
    fn new_untitled_tab_numbers_after_the_first() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.new_untitled_tab();
        assert_eq!(app.tabs[0].title, "Untitled");
        assert_eq!(app.tabs[1].title, "Untitled-2");
        assert_eq!(app.active_tab, Some(1));
    }

    #[test]
    fn cycle_tab_wraps_forward_and_backward() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.new_untitled_tab();
        app.new_untitled_tab();
        app.active_tab = Some(0);

        app.cycle_tab(1);
        assert_eq!(app.active_tab, Some(1));
        app.cycle_tab(1);
        assert_eq!(app.active_tab, Some(2));
        app.cycle_tab(1);
        assert_eq!(app.active_tab, Some(0));

        app.cycle_tab(-1);
        assert_eq!(app.active_tab, Some(2));
    }

    #[test]
    fn cycle_tab_with_no_tabs_or_no_active_tab_is_a_noop() {
        let mut app = app_without_gui();
        app.cycle_tab(1);
        assert_eq!(app.active_tab, None);

        app.new_untitled_tab();
        app.active_tab = None;
        app.cycle_tab(1);
        assert_eq!(app.active_tab, None);
    }

    #[test]
    fn open_project_error_sets_error_message() {
        let mut app = app_without_gui();
        app.open_project(
            Path::new("/definitely/does/not/exist/anywhere"),
            &egui::Context::default(),
        );
        assert!(app.error.is_some());
        assert!(app.project.is_none());
    }

    #[test]
    fn create_and_open_project_populate_tree() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_without_gui();
        app.open_project(dir.path(), &egui::Context::default());
        assert!(app.project.is_some());
        wait_until(|| app.poll_tree_scan());
        assert!(app.tree.is_some());
        assert!(app.error.is_none());
    }

    #[test]
    fn open_project_that_is_not_a_git_repo_leaves_git_panel_empty() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_without_gui();
        app.open_project(dir.path(), &egui::Context::default());
        assert!(!app.git.is_repo());
    }

    #[test]
    fn refresh_tree_picks_up_git_init_run_outside_the_app() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_without_gui();
        app.open_project(dir.path(), &egui::Context::default());
        assert!(!app.git.is_repo());

        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        app.refresh_tree();
        assert!(app.git.is_repo());
    }

    // ---- editor-git-gutter ----

    fn git_run(dir: &Path, args: &[&str]) {
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

    fn git_init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git_run(dir.path(), &["init", "-q"]);
        dir
    }

    fn git_commit(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
        git_run(dir, &["add", "."]);
        git_run(dir, &["commit", "-q", "-m", "commit"]);
    }

    #[test]
    fn sync_git_gutter_reflects_a_saved_working_tree_change() {
        let dir = git_init_repo();
        git_commit(dir.path(), "f.txt", "a\nb\nc\n");
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "a\nB\nc\n").unwrap();

        let mut app = app_without_gui();
        app.git.refresh(dir.path());
        app.open_file(&file);

        app.sync_git_gutter();

        assert_eq!(app.git_gutter.len(), 1);
        assert_eq!(app.git_gutter[0].line, 1);
        assert_eq!(app.git_gutter_path, Some(file.canonicalize().unwrap()));
    }

    #[test]
    fn sync_git_gutter_clears_marks_while_the_buffer_is_dirty() {
        let dir = git_init_repo();
        git_commit(dir.path(), "f.txt", "a\nb\nc\n");
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "a\nB\nc\n").unwrap();

        let mut app = app_without_gui();
        app.git.refresh(dir.path());
        app.open_file(&file);
        app.tabs[0].buffer.insert(0, "x");

        app.sync_git_gutter();

        assert!(app.git_gutter.is_empty());
        assert!(app.git_gutter_path.is_none());
    }

    #[test]
    fn sync_git_gutter_with_no_active_tab_clears_marks() {
        let mut app = app_without_gui();
        app.git_gutter = vec![crate::editor::GutterMark {
            line: 0,
            kind: crate::editor::GutterMarkKind::Added,
        }];
        app.git_gutter_path = Some(PathBuf::from("/stale.rs"));

        app.sync_git_gutter();

        assert!(app.git_gutter.is_empty());
        assert!(app.git_gutter_path.is_none());
    }

    #[test]
    fn handle_git_gutter_click_opens_the_popup_on_the_clicked_line() {
        let mut app = app_without_gui();
        let output = editor::EditorOutput {
            cursor_offset: 0,
            changed: false,
            hovered_word: None,
            clicked_link: None,
            git_gutter_clicked_line: Some(3),
            blame_clicked_line: None,
        };

        app.handle_git_gutter_click(&output);

        assert_eq!(app.git_gutter_popup_line, Some(3));
    }

    fn blame_annotation(line: usize, run_len: usize, commit_id: &str) -> BlameAnnotation {
        BlameAnnotation {
            line,
            run_len,
            commit_id: commit_id.to_string(),
            short_id: commit_id[..commit_id.len().min(7)].to_string(),
            author: "Test".to_string(),
            timestamp: 0,
            summary: "commit".to_string(),
        }
    }

    #[test]
    fn handle_blame_click_opens_the_popup_for_the_covering_annotation() {
        let mut app = app_without_gui();
        app.tabs.push(Tab::untitled("Untitled".to_string()));
        app.active_tab = Some(0);
        app.tabs[0].blame = Some(vec![
            blame_annotation(0, 2, "aaa"),
            blame_annotation(2, 1, "bbb"),
        ]);
        let output = editor::EditorOutput {
            cursor_offset: 0,
            changed: false,
            hovered_word: None,
            clicked_link: None,
            git_gutter_clicked_line: None,
            blame_clicked_line: Some(2),
        };

        app.handle_blame_click(0, &output);

        assert_eq!(app.blame_popup_commit_id, Some("bbb".to_string()));
    }

    #[test]
    fn handle_blame_click_is_a_noop_when_no_annotation_covers_the_line() {
        let mut app = app_without_gui();
        app.tabs.push(Tab::untitled("Untitled".to_string()));
        app.active_tab = Some(0);
        app.tabs[0].blame = Some(vec![blame_annotation(0, 2, "aaa")]);
        let output = editor::EditorOutput {
            cursor_offset: 0,
            changed: false,
            hovered_word: None,
            clicked_link: None,
            git_gutter_clicked_line: None,
            blame_clicked_line: Some(5),
        };

        app.handle_blame_click(0, &output);

        assert_eq!(app.blame_popup_commit_id, None);
    }

    #[test]
    fn toggle_blame_annotations_turns_off_an_already_on_tab() {
        let mut app = app_without_gui();
        app.tabs.push(Tab::untitled("Untitled".to_string()));
        app.active_tab = Some(0);
        app.tabs[0].blame = Some(vec![blame_annotation(0, 1, "aaa")]);

        app.toggle_blame_annotations();

        assert!(app.tabs[0].blame.is_none());
    }

    #[test]
    fn toggle_blame_annotations_is_a_noop_for_an_untitled_tab() {
        let mut app = app_without_gui();
        app.tabs.push(Tab::untitled("Untitled".to_string()));
        app.active_tab = Some(0);

        app.toggle_blame_annotations();

        assert!(app.tabs[0].blame.is_none());
    }

    #[test]
    fn toggle_blame_annotations_populates_from_a_real_repo_then_toggling_again_clears_it() {
        let dir = git_init_repo();
        git_commit(
            dir.path(),
            "f.txt",
            "a
b
c
",
        );
        let file = dir.path().join("f.txt");

        let mut app = app_without_gui();
        app.git.refresh(dir.path());
        app.open_file(&file);

        app.toggle_blame_annotations();
        let idx = app.active_tab.unwrap();
        assert_eq!(app.tabs[idx].blame.as_ref().map(Vec::len), Some(1));

        app.toggle_blame_annotations();
        assert!(app.tabs[idx].blame.is_none());
    }

    #[test]
    fn saving_a_tab_with_blame_on_refreshes_its_annotations() {
        let dir = git_init_repo();
        git_commit(
            dir.path(),
            "f.txt",
            "a
b
c
",
        );
        let file = dir.path().join("f.txt");

        let mut app = app_without_gui();
        app.git.refresh(dir.path());
        app.open_file(&file);
        app.toggle_blame_annotations();
        let idx = app.active_tab.unwrap();
        let before = app.tabs[idx].blame.clone();
        assert!(before.is_some());

        app.tabs[idx].buffer.insert(
            0, "z
",
        );
        app.save_tab_at(idx);

        // Same content shape (still one commit's worth of lines), but the
        // save path re-ran `refresh_blame_if_on` rather than leaving the
        // pre-save cache stale -- proven by the tab having fresh blame at
        // all once the save has gone through a repo with a real HEAD.
        assert!(app.tabs[idx].blame.is_some());
    }

    #[test]
    fn reloading_a_tab_with_blame_on_refreshes_its_annotations() {
        let dir = git_init_repo();
        git_commit(
            dir.path(),
            "f.txt",
            "a
b
c
",
        );
        let file = dir.path().join("f.txt");

        let mut app = app_without_gui();
        app.git.refresh(dir.path());
        app.open_file(&file);
        app.toggle_blame_annotations();
        let idx = app.active_tab.unwrap();

        std::fs::write(
            &file, "a
b
c
",
        )
        .unwrap();
        app.reload_tab_from_disk(idx);

        assert!(app.tabs[idx].blame.is_some());
    }

    #[test]
    fn refresh_blame_if_on_is_a_noop_when_blame_is_off() {
        let mut app = app_without_gui();
        app.tabs.push(Tab::untitled("Untitled".to_string()));
        app.refresh_blame_if_on(0);
        assert!(app.tabs[0].blame.is_none());
    }

    #[test]
    fn filtered_branch_rows_matches_by_fuzzy_query_and_sorts_best_first() {
        let mut app = app_without_gui();
        app.git.branches = vec![
            ide_core::BranchInfo {
                name: "feature-login".to_string(),
                is_head: false,
            },
            ide_core::BranchInfo {
                name: "main".to_string(),
                is_head: true,
            },
        ];

        let all = app.filtered_branch_rows();
        assert_eq!(all.len(), 2);

        app.git.branches_popup.filter = "main".to_string();
        let rows = app.filtered_branch_rows();
        assert_eq!(rows, vec![("main".to_string(), true)]);
    }

    #[test]
    fn branches_popup_move_selection_wraps_and_is_a_noop_when_empty() {
        let mut app = app_without_gui();
        app.branches_popup_move_selection(1);
        assert_eq!(app.git.branches_popup.selected, 0);

        app.git.branches = vec![
            ide_core::BranchInfo {
                name: "a".to_string(),
                is_head: false,
            },
            ide_core::BranchInfo {
                name: "b".to_string(),
                is_head: false,
            },
        ];
        app.branches_popup_move_selection(-1);
        assert_eq!(app.git.branches_popup.selected, 1);
    }

    #[test]
    fn branches_popup_confirm_checks_out_the_selected_non_head_branch() {
        let dir = git_init_repo();
        git_commit(
            dir.path(),
            "f.txt",
            "a
",
        );
        git_run(dir.path(), &["branch", "feature"]);

        let mut app = app_without_gui();
        app.project = Some(ide_core::Project::open(dir.path()).unwrap());
        app.git.open_branches_popup(dir.path());
        app.git.branches_popup.selected = app
            .filtered_branch_rows()
            .iter()
            .position(|(name, _)| name == "feature")
            .unwrap();

        app.branches_popup_confirm();

        assert_eq!(app.git.current_branch.as_deref(), Some("feature"));
    }

    #[test]
    fn branches_popup_confirm_on_the_head_row_is_a_noop() {
        let dir = git_init_repo();
        git_commit(
            dir.path(),
            "f.txt",
            "a
",
        );

        let mut app = app_without_gui();
        app.project = Some(ide_core::Project::open(dir.path()).unwrap());
        app.git.open_branches_popup(dir.path());
        // Only branch is HEAD's own -- index 0.
        app.git.branches_popup.selected = 0;
        let before = app.git.current_branch.clone();

        app.branches_popup_confirm();

        assert_eq!(app.git.current_branch, before);
    }

    #[test]
    fn is_command_enabled_git_branches_needs_a_project() {
        let app = app_without_gui();
        assert!(!app.is_command_enabled(CommandAction::GitBranches));
    }

    #[test]
    fn is_command_enabled_toggle_blame_annotations_needs_a_saved_tab() {
        let mut app = app_without_gui();
        assert!(!app.is_command_enabled(CommandAction::ToggleBlameAnnotations));
        app.tabs.push(Tab::untitled("Untitled".to_string()));
        app.active_tab = Some(0);
        assert!(!app.is_command_enabled(CommandAction::ToggleBlameAnnotations));
    }

    #[test]
    fn run_command_git_branches_opens_the_popup_when_a_project_is_open() {
        let dir = git_init_repo();
        git_commit(
            dir.path(),
            "f.txt",
            "a
",
        );
        let mut app = app_without_gui();
        app.project = Some(ide_core::Project::open(dir.path()).unwrap());
        let ctx = egui::Context::default();

        app.run_command(CommandAction::GitBranches, &ctx);

        assert!(app.git.branches_popup.open);
    }

    #[test]
    fn run_command_toggle_blame_annotations_dispatches() {
        let dir = git_init_repo();
        git_commit(
            dir.path(),
            "f.txt",
            "a
b
",
        );
        let file = dir.path().join("f.txt");
        let mut app = app_without_gui();
        app.git.refresh(dir.path());
        app.open_file(&file);
        let ctx = egui::Context::default();

        app.run_command(CommandAction::ToggleBlameAnnotations, &ctx);

        let idx = app.active_tab.unwrap();
        assert!(app.tabs[idx].blame.is_some());
    }

    #[test]
    fn trigger_revert_hunk_applies_the_change_and_closes_the_popup() {
        let dir = git_init_repo();
        git_commit(dir.path(), "f.txt", "a\nb\nc\n");
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "a\nB\nc\n").unwrap();

        let mut app = app_without_gui();
        app.git.refresh(dir.path());
        app.open_file(&file);
        app.sync_git_gutter();
        app.git_gutter_popup_line = Some(1);

        app.trigger_revert_hunk();

        assert!(app.git_gutter_popup_line.is_none());
        assert_eq!(app.tabs[0].buffer.text(), "a\nb\nc\n");
    }

    #[test]
    fn trigger_revert_hunk_with_no_popup_open_is_a_noop() {
        let mut app = app_without_gui();
        app.trigger_revert_hunk();
        assert!(app.git_gutter_popup_line.is_none());
    }

    #[test]
    fn trigger_show_diff_for_gutter_switches_to_source_control_and_loads_the_diff() {
        let dir = git_init_repo();
        git_commit(dir.path(), "f.txt", "a\nb\nc\n");
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "a\nB\nc\n").unwrap();

        let mut app = app_without_gui();
        app.git.refresh(dir.path());
        app.open_file(&file);
        app.git_gutter_popup_line = Some(1);

        app.trigger_show_diff_for_gutter();

        assert!(app.git_gutter_popup_line.is_none());
        assert_eq!(app.view_mode, ViewMode::SourceControl);
        assert!(app.git.diff.is_some());
    }

    #[test]
    fn open_file_focuses_existing_tab_instead_of_duplicating() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "hi").unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.open_file(&file);

        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.active_tab, Some(0));
    }

    #[test]
    fn open_missing_file_sets_error_and_no_tab() {
        let mut app = app_without_gui();
        app.open_file(Path::new("/definitely/does/not/exist.txt"));
        assert!(app.error.is_some());
        assert!(app.tabs.is_empty());
    }

    #[test]
    fn request_close_clean_tab_closes_immediately() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.request_close_tab(0);
        assert!(app.tabs.is_empty());
        assert!(app.pending_confirm.is_none());
    }

    #[test]
    fn request_close_dirty_tab_arms_confirm() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.tabs[0].buffer.insert(0, "x");
        app.request_close_tab(0);
        assert_eq!(app.tabs.len(), 1, "not closed yet, awaiting confirm");
        assert_eq!(app.pending_confirm, Some(PendingConfirm::CloseTab(0)));
    }

    #[test]
    fn confirm_discard_closes_the_tab() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.tabs[0].buffer.insert(0, "x");
        app.request_close_tab(0);
        app.confirm_discard();
        assert!(app.tabs.is_empty());
        assert!(app.pending_confirm.is_none());
    }

    #[test]
    fn cancel_confirm_keeps_the_tab_open() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.tabs[0].buffer.insert(0, "x");
        app.request_close_tab(0);
        app.cancel_confirm();
        assert_eq!(app.tabs.len(), 1);
        assert!(app.pending_confirm.is_none());
    }

    #[test]
    fn request_quit_with_no_dirty_tabs_allows_immediate_quit() {
        let mut app = app_without_gui();
        app.new_untitled_tab(); // clean
        assert!(app.request_quit());
        assert!(app.pending_confirm.is_none());
    }

    #[test]
    fn request_quit_with_dirty_tabs_arms_confirm_and_blocks() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.tabs[0].buffer.insert(0, "x");
        assert!(!app.request_quit());
        assert_eq!(app.pending_confirm, Some(PendingConfirm::Quit));
    }

    #[test]
    fn quit_confirm_prompts_once_per_dirty_tab_then_quits() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.tabs[0].buffer.insert(0, "x");
        app.new_untitled_tab();
        app.tabs[1].buffer.insert(0, "y");

        assert!(!app.request_quit());
        assert!(!app.should_quit);

        app.confirm_discard(); // closes first dirty tab, one remains
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.pending_confirm, Some(PendingConfirm::Quit));
        assert!(!app.should_quit);

        app.confirm_discard(); // closes the last dirty tab
        assert!(app.tabs.is_empty());
        assert!(app.pending_confirm.is_none());
        assert!(app.should_quit);
    }

    #[test]
    fn undo_redo_active_tab_walks_the_buffer_history() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.tabs[0].buffer.insert(0, "hello");

        app.undo_active();
        assert_eq!(app.tabs[0].buffer.text(), "");

        app.redo_active();
        assert_eq!(app.tabs[0].buffer.text(), "hello");
    }

    #[test]
    fn save_active_with_no_path_returns_none() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        assert!(app.save_active().is_none());
    }

    #[test]
    fn save_active_with_path_writes_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "orig").unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.tabs[0].buffer.insert(4, "!");
        let result = app.save_active();
        assert!(matches!(result, Some(Ok(()))));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "orig!");
    }

    #[test]
    fn save_active_as_sets_path_and_title() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("saved.txt");

        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.tabs[0].buffer.insert(0, "content");
        let result = app.save_active_as(&target);

        assert!(matches!(result, Some(Ok(()))));
        assert_eq!(app.tabs[0].title, "saved.txt");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "content");
    }

    // ---- A4b: EditorConfig wiring ----

    #[test]
    fn opening_a_file_under_a_project_applies_its_editorconfig_indent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".editorconfig"),
            "[*]\nindent_style = tab\nindent_size = 2\n",
        )
        .unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "hi").unwrap();

        let mut app = app_without_gui();
        app.open_project(dir.path(), &egui::Context::default());
        app.open_file(&file);

        let unit = app.tabs[0].editor.indent();
        assert_eq!(unit.style, ide_core::IndentStyle::Tabs);
        assert_eq!(unit.width, 2);
    }

    #[test]
    fn a_tab_outside_any_project_keeps_the_default_indent() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        assert_eq!(app.tabs[0].editor.indent(), IndentUnit::default());
    }

    #[test]
    fn saving_applies_the_editorconfig_final_newline_in_one_undo_step() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".editorconfig"),
            "[*]\ninsert_final_newline = true\n",
        )
        .unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "no newline").unwrap();

        let mut app = app_without_gui();
        app.open_project(dir.path(), &egui::Context::default());
        app.open_file(&file);

        let before = app.tabs[0].buffer.text().to_string();
        let result = app.save_active();
        assert!(matches!(result, Some(Ok(()))));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "no newline\n");
        assert_eq!(app.tabs[0].buffer.text(), "no newline\n");

        assert!(app.tabs[0].buffer.undo());
        assert_eq!(app.tabs[0].buffer.text(), before);
    }

    #[test]
    fn saving_an_unsupported_charset_notices_once_per_tab() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".editorconfig"), "[*]\ncharset = latin1\n").unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "hi").unwrap();

        let mut app = app_without_gui();
        app.open_project(dir.path(), &egui::Context::default());
        app.open_file(&file);

        assert!(app.error.is_none());
        let result = app.save_active();
        assert!(matches!(result, Some(Ok(()))));
        assert!(app.error.is_some());
        // Written as UTF-8, not mojibake -- the property is recognised but
        // never applied (§3.6's charset rule).
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hi");

        app.error = None;
        let result = app.save_active();
        assert!(matches!(result, Some(Ok(()))));
        assert!(
            app.error.is_none(),
            "the notice must not repeat on a later save of the same tab"
        );
    }

    // ---- G6: file watcher ----

    fn poll_watcher_until(
        app: &mut IdeApp,
        deadline: std::time::Duration,
        pred: impl Fn(&IdeApp) -> bool,
    ) {
        let start = std::time::Instant::now();
        loop {
            app.poll_watcher();
            // A `WatchEvent::TreeChanged` dispatched by `poll_watcher`
            // above only *starts* a background scan (`async-tree-scan.md`
            // §3.1) -- draining it here mirrors the real per-frame poll
            // order in `render.rs` and lets predicates that inspect
            // `app.tree` observe the refreshed tree.
            app.poll_tree_scan();
            if pred(app) || start.elapsed() >= deadline {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn poll_watcher_on_tree_changed_refreshes_the_tree() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_without_gui();
        app.open_project(dir.path(), &egui::Context::default());
        poll_watcher_until(&mut app, std::time::Duration::from_secs(1), |_| false);

        std::fs::write(dir.path().join("new.txt"), "hi").unwrap();

        poll_watcher_until(&mut app, std::time::Duration::from_secs(5), |app| {
            app.tree
                .as_ref()
                .is_some_and(|t| t.children.iter().any(|c| c.name == "new.txt"))
        });

        assert!(app
            .tree
            .as_ref()
            .unwrap()
            .children
            .iter()
            .any(|c| c.name == "new.txt"));
    }

    #[test]
    fn file_modified_reloads_a_clean_tab_silently() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "v0").unwrap();

        let mut app = app_without_gui();
        app.open_project(dir.path(), &egui::Context::default());
        app.open_file(&file);
        let idx = app.active_tab.unwrap();
        assert!(!app.tabs[idx].buffer.is_dirty());

        std::fs::write(&file, "v1 from outside").unwrap();

        poll_watcher_until(&mut app, std::time::Duration::from_secs(5), |app| {
            app.tabs[idx].buffer.text() == "v1 from outside"
        });

        assert_eq!(app.tabs[idx].buffer.text(), "v1 from outside");
        assert!(app.tabs[idx].external_change.is_none());
    }

    #[test]
    fn file_modified_notices_a_dirty_tab_without_touching_it() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "v0").unwrap();

        let mut app = app_without_gui();
        app.open_project(dir.path(), &egui::Context::default());
        app.open_file(&file);
        let idx = app.active_tab.unwrap();
        app.tabs[idx].buffer.insert(0, "unsaved ");
        assert!(app.tabs[idx].buffer.is_dirty());

        std::fs::write(&file, "v1 from outside").unwrap();

        poll_watcher_until(&mut app, std::time::Duration::from_secs(5), |app| {
            app.tabs[idx].external_change == Some(ExternalChange::Modified)
        });

        assert_eq!(
            app.tabs[idx].external_change,
            Some(ExternalChange::Modified)
        );
        assert_eq!(app.tabs[idx].buffer.text(), "unsaved v0");
    }

    #[test]
    fn file_removed_sets_deleted_regardless_of_dirty_state() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "v0").unwrap();

        let mut app = app_without_gui();
        app.open_project(dir.path(), &egui::Context::default());
        app.open_file(&file);
        let idx = app.active_tab.unwrap();
        app.tabs[idx].buffer.insert(0, "unsaved ");
        assert!(app.tabs[idx].buffer.is_dirty());

        std::fs::remove_file(&file).unwrap();

        poll_watcher_until(&mut app, std::time::Duration::from_secs(5), |app| {
            app.tabs[idx].external_change == Some(ExternalChange::Deleted)
        });

        assert_eq!(app.tabs[idx].external_change, Some(ExternalChange::Deleted));
    }

    #[test]
    fn reload_active_from_disk_replaces_text_and_clears_external_change() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "v0").unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        let idx = app.active_tab.unwrap();
        app.tabs[idx].buffer.insert(0, "unsaved ");
        app.tabs[idx].external_change = Some(ExternalChange::Modified);

        std::fs::write(&file, "v1 on disk").unwrap();
        app.reload_active_from_disk();

        assert_eq!(app.tabs[idx].buffer.text(), "v1 on disk");
        assert!(app.tabs[idx].external_change.is_none());
        assert!(!app.tabs[idx].buffer.is_dirty());
    }

    #[test]
    fn dismiss_external_change_clears_without_touching_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "v0").unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        let idx = app.active_tab.unwrap();
        app.tabs[idx].buffer.insert(0, "unsaved ");
        app.tabs[idx].external_change = Some(ExternalChange::Modified);

        app.dismiss_external_change();

        assert!(app.tabs[idx].external_change.is_none());
        assert_eq!(app.tabs[idx].buffer.text(), "unsaved v0");
    }

    #[test]
    fn save_active_suppresses_the_watcher_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "v0").unwrap();

        let mut app = app_without_gui();
        app.open_project(dir.path(), &egui::Context::default());
        app.open_file(&file);
        let idx = app.active_tab.unwrap();
        poll_watcher_until(&mut app, std::time::Duration::from_secs(2), |_| false); // drain initial noise

        app.tabs[idx].buffer.insert(0, "saved via app ");
        let result = app.save_active();
        assert!(matches!(result, Some(Ok(()))));
        assert!(!app.tabs[idx].buffer.is_dirty());

        // Dirty the tab again immediately: a *not*-suppressed watcher event
        // for this save would otherwise be indistinguishable from success,
        // since a clean tab just silently reloads either way.
        app.tabs[idx].buffer.insert(0, "further unsaved edit ");
        assert!(app.tabs[idx].buffer.is_dirty());

        poll_watcher_until(&mut app, std::time::Duration::from_secs(2), |_| false);

        assert!(
            app.tabs[idx].external_change.is_none(),
            "save_active's own write must have been suppressed, not surfaced as an external change"
        );
    }

    #[test]
    fn watch_error_surfaces_through_self_error_without_failing_project_open() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("proj");
        let project = Project::create(&target).unwrap();
        std::fs::remove_dir_all(&target).unwrap();

        let mut app = app_without_gui();
        app.load_project(project, &egui::Context::default());

        assert!(
            app.project.is_some(),
            "project open must not fail even if the watcher can't start"
        );
        assert!(app.watcher.is_none());
        assert!(app.error.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn open_file_canonicalizes_so_a_symlinked_path_focuses_the_same_tab() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.txt");
        std::fs::write(&real, "hi").unwrap();
        let link_dir = dir.path().join("link_dir");
        std::os::unix::fs::symlink(dir.path(), &link_dir).unwrap();
        let via_symlink = link_dir.join("real.txt");

        let mut app = app_without_gui();
        app.open_file(&real);
        app.open_file(&via_symlink);

        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.active_tab, Some(0));
    }

    #[test]
    fn close_tab_now_adjusts_active_index() {
        let mut app = app_without_gui();
        app.new_untitled_tab(); // idx 0
        app.new_untitled_tab(); // idx 1, active
        app.close_tab_now(0);
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.active_tab, Some(0));
    }

    // ---- Syntax highlighting (Tab::syntax/tokens) ----

    #[test]
    fn from_buffer_detects_syntax_and_tokenizes_initial_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.json");
        std::fs::write(&path, "42").unwrap();

        let tab = Tab::from_buffer(Buffer::open(&path).unwrap());

        assert!(tab.syntax.is_some());
        assert_eq!(
            tab.buffer.text_buffer().tokens(),
            vec![Token {
                range: 0..2,
                kind: ide_core::TokenKind::Number,
            }]
        );
    }

    #[test]
    fn from_buffer_with_unrecognized_extension_has_no_syntax_or_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logo.png");
        std::fs::write(&path, "not really an image").unwrap();

        let tab = Tab::from_buffer(Buffer::open(&path).unwrap());

        assert!(tab.syntax.is_none());
        assert!(tab.buffer.text_buffer().tokens().is_empty());
    }

    #[test]
    fn from_buffer_detects_rust_syntax() {
        // Pins from the UI side what the fixture above used to assert with
        // a .rs path: Rust is a built-in language now.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.rs");
        std::fs::write(&path, "fn main() {}").unwrap();

        let tab = Tab::from_buffer(Buffer::open(&path).unwrap());

        assert_eq!(tab.syntax.unwrap().name, "Rust");
        assert!(!tab.buffer.text_buffer().tokens().is_empty());
    }

    #[test]
    fn from_buffer_detects_syntax_for_extensionless_filenames() {
        // These resolve only through `syntax_for_path`'s filename
        // dimension -- `Path::extension()` is None for all three, so the
        // old extension-only lookup could never have matched them.
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in [
            ("Makefile", "build: main.o\n"),
            ("Dockerfile", "FROM ubuntu\n"),
            (".env", "FOO=bar\n"),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, content).unwrap();

            let tab = Tab::from_buffer(Buffer::open(&path).unwrap());

            assert!(
                path.extension().is_none(),
                "{name} should have no extension"
            );
            assert!(tab.syntax.is_some(), "{name} should resolve a syntax");
            assert!(
                !tab.buffer.text_buffer().tokens().is_empty(),
                "{name} should produce tokens"
            );
        }
    }

    #[test]
    fn from_buffer_detects_syntax_for_filename_prefix_variants() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Dockerfile.dev");
        std::fs::write(&path, "FROM ubuntu\n").unwrap();

        let tab = Tab::from_buffer(Buffer::open(&path).unwrap());

        assert_eq!(tab.syntax.unwrap().name, "Dockerfile");
    }

    #[test]
    fn untitled_tab_has_no_syntax_or_tokens() {
        let tab = Tab::untitled("Untitled".to_string());
        assert!(tab.syntax.is_none());
        assert!(tab.buffer.text_buffer().tokens().is_empty());
    }

    #[test]
    fn editing_a_tab_retokenizes_incrementally() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.json");
        std::fs::write(&path, "1").unwrap();
        let mut tab = Tab::from_buffer(Buffer::open(&path).unwrap());
        assert_eq!(tab.buffer.text_buffer().tokens().len(), 1);

        tab.buffer.insert(1, "2");

        assert_eq!(
            tab.buffer.text_buffer().tokens(),
            vec![Token {
                range: 0..2,
                kind: ide_core::TokenKind::Number,
            }]
        );
    }

    // ---- Rust-project detection / LSP wiring ----

    fn write_cargo_toml(dir: &Path) {
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
    }

    #[test]
    fn is_rust_project_reflects_cargo_toml_presence() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_without_gui();
        app.open_project(dir.path(), &egui::Context::default());
        assert!(!app.is_rust_project());

        write_cargo_toml(dir.path());
        app.refresh_tree();
        assert!(app.is_rust_project());
    }

    #[test]
    fn load_project_with_cargo_toml_attempts_to_start_lsp() {
        let dir = tempfile::tempdir().unwrap();
        write_cargo_toml(dir.path());
        let mut app = app_without_gui();
        app.open_project(dir.path(), &egui::Context::default());
        assert!(app.is_rust_project());
        wait_until(|| app.poll_tree_scan());
        // Whether or not `rust-analyzer` happens to be installed in
        // whatever environment runs this test, `load_project` must have
        // attempted to start it: either it's running, or the attempt
        // failed and left a message -- distinct from the no-Cargo.toml
        // case below, where neither ever becomes true.
        assert!(app.lsp.is_running() || app.lsp.server_error.is_some());
    }

    #[test]
    fn load_project_without_cargo_toml_does_not_attempt_to_start_lsp() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_without_gui();
        app.open_project(dir.path(), &egui::Context::default());
        assert!(!app.is_rust_project());
        wait_until(|| app.poll_tree_scan());
        assert!(!app.lsp.is_running());
        assert!(app.lsp.server_error.is_none());
    }

    #[test]
    fn load_project_switching_away_from_a_rust_project_clears_lsp_state() {
        let rust_dir = tempfile::tempdir().unwrap();
        write_cargo_toml(rust_dir.path());
        let plain_dir = tempfile::tempdir().unwrap();

        let mut app = app_without_gui();
        app.open_project(rust_dir.path(), &egui::Context::default());
        wait_until(|| app.poll_tree_scan());
        assert!(app.lsp.is_running() || app.lsp.server_error.is_some());

        app.open_project(plain_dir.path(), &egui::Context::default());
        wait_until(|| app.poll_tree_scan());
        // `load_project` must have called `lsp.stop()` for the new,
        // non-Rust project: no client running, and any leftover error
        // message from the first project's start attempt is cleared too.
        assert!(!app.lsp.is_running());
        assert!(app.lsp.server_error.is_none());
    }

    #[test]
    fn restart_lsp_with_no_project_is_a_noop() {
        let mut app = app_without_gui();
        app.restart_lsp();
        assert!(!app.lsp.is_running());
    }

    // ---- project-settings.md: per-project preferences/workspace ----

    #[test]
    fn flush_then_load_project_settings_round_trips_preferences() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_without_gui();
        app.theme = Theme::Light;
        app.custom_languages = vec![go_config()];
        app.format_on_save = true;
        app.dismissed_language_suggestions = vec!["go.mod".to_string()];

        app.flush_project_settings(dir.path());

        let mut reloaded = app_without_gui();
        reloaded.load_project_settings(dir.path(), &egui::Context::default());

        assert_eq!(reloaded.theme, Theme::Light);
        assert_eq!(reloaded.custom_languages, vec![go_config()]);
        assert!(reloaded.format_on_save);
        assert_eq!(
            reloaded.dismissed_language_suggestions,
            vec!["go.mod".to_string()]
        );
    }

    #[test]
    fn load_project_settings_truncates_a_custom_languages_array_past_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let ide_dir = dir.path().join(".ide");
        std::fs::create_dir_all(&ide_dir).unwrap();
        let entries: Vec<String> = (0..(MAX_CUSTOM_LANGUAGES + 50))
            .map(|i| {
                format!(
                    r#"{{"name":"L{i}","extension":"e{i}","command":"cmd{i}","args":[],"extra_extensions":[]}}"#
                )
            })
            .collect();
        let json = format!(r#"{{"custom_languages":[{}]}}"#, entries.join(","));
        std::fs::write(ide_dir.join("preferences.json"), json).unwrap();

        let mut app = app_without_gui();
        app.load_project_settings(dir.path(), &egui::Context::default());

        assert_eq!(app.custom_languages.len(), MAX_CUSTOM_LANGUAGES);
        // Truncation keeps the first N entries, not an arbitrary subset.
        assert_eq!(app.custom_languages[0].name, "L0");
    }

    #[test]
    fn load_project_settings_truncates_a_dismissed_suggestions_array_past_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let ide_dir = dir.path().join(".ide");
        std::fs::create_dir_all(&ide_dir).unwrap();
        let entries: Vec<String> = (0..(MAX_DISMISSED_LANGUAGE_SUGGESTIONS + 50))
            .map(|i| format!(r#""marker{i}""#))
            .collect();
        let json = format!(
            r#"{{"dismissed_language_suggestions":[{}]}}"#,
            entries.join(",")
        );
        std::fs::write(ide_dir.join("preferences.json"), json).unwrap();

        let mut app = app_without_gui();
        app.load_project_settings(dir.path(), &egui::Context::default());

        assert_eq!(
            app.dismissed_language_suggestions.len(),
            MAX_DISMISSED_LANGUAGE_SUGGESTIONS
        );
        assert_eq!(app.dismissed_language_suggestions[0], "marker0");
    }

    #[test]
    fn load_project_settings_defaults_when_no_ide_directory_exists() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_without_gui();
        app.theme = Theme::Light;

        app.load_project_settings(dir.path(), &egui::Context::default());

        assert_eq!(app.theme, Theme::Dark);
        assert!(app.custom_languages.is_empty());
        assert!(!app.format_on_save);
    }

    #[test]
    fn project_switch_preserves_each_projects_own_theme() {
        let project_a = tempfile::tempdir().unwrap();
        let project_b = tempfile::tempdir().unwrap();
        let mut app = app_without_gui();

        app.open_project(project_a.path(), &egui::Context::default());
        assert_eq!(app.theme, Theme::Dark);
        app.theme = Theme::Light;

        app.open_project(project_b.path(), &egui::Context::default());
        assert_eq!(
            app.theme,
            Theme::Dark,
            "project B has no .ide/ yet, must not inherit A's in-memory theme"
        );

        app.open_project(project_a.path(), &egui::Context::default());
        assert_eq!(
            app.theme,
            Theme::Light,
            "switching back to A must restore A's own flushed theme"
        );
    }

    #[test]
    fn project_switch_clears_previously_open_tabs() {
        let project_a = tempfile::tempdir().unwrap();
        std::fs::write(project_a.path().join("a.txt"), "hello").unwrap();
        let project_b = tempfile::tempdir().unwrap();
        let mut app = app_without_gui();

        app.open_project(project_a.path(), &egui::Context::default());
        app.open_file(&project_a.path().join("a.txt"));
        assert_eq!(app.tabs.len(), 1);

        app.open_project(project_b.path(), &egui::Context::default());

        assert!(
            app.tabs.is_empty(),
            "switching projects must not carry the old project's tabs over"
        );
    }

    #[test]
    fn workspace_restore_skips_a_deleted_file_and_restores_cursor_offset() {
        let dir = tempfile::tempdir().unwrap();
        // `flush_project_settings` is called directly below (bypassing
        // `save()`/`load_project`, which always pass an already-canonical
        // `project.root()`) -- canonicalize here too, since on macOS
        // `tempfile::tempdir()` returns a path under a symlink
        // (`/var/...` -> `/private/var/...`) and `Tab::buffer.path()` is
        // always canonical (`canonicalize_best_effort`), so a mismatched
        // root would make every `strip_prefix` in `flush_project_settings`
        // fail silently.
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let lib_rs = root.join("lib.rs");
        let main_rs = root.join("main.rs");
        std::fs::write(&lib_rs, "0123456789").unwrap();
        std::fs::write(&main_rs, "fn main() {}").unwrap();

        let mut app = app_without_gui();
        app.open_project(&root, &egui::Context::default());
        app.open_file(&lib_rs);
        app.tabs[0]
            .buffer
            .text_buffer_mut()
            .set_selections(Selections::single(Selection::caret(4)));
        app.open_file(&main_rs);
        app.active_tab = Some(1);
        app.flush_project_settings(&root);

        std::fs::remove_file(&main_rs).unwrap();

        let mut reopened = app_without_gui();
        reopened.open_project(&root, &egui::Context::default());

        assert_eq!(
            reopened.tabs.len(),
            1,
            "the deleted main.rs must be skipped"
        );
        assert_eq!(
            reopened.tabs[0].buffer.path().and_then(|p| p.file_name()),
            Some(std::ffi::OsStr::new("lib.rs"))
        );
        assert_eq!(
            reopened.tabs[0]
                .buffer
                .text_buffer()
                .selections()
                .primary()
                .head,
            4
        );
        // The persisted `active_path` ("main.rs") matches no live tab, so
        // this falls back to the one tab that *did* restore (doc §3.3).
        assert_eq!(reopened.active_tab, Some(0));
    }

    #[test]
    fn workspace_restore_clamps_a_cursor_offset_past_the_shrunk_files_length() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let file = root.join("a.txt");
        std::fs::write(&file, "0123456789").unwrap();

        let mut app = app_without_gui();
        app.open_project(&root, &egui::Context::default());
        app.open_file(&file);
        app.tabs[0]
            .buffer
            .text_buffer_mut()
            .set_selections(Selections::single(Selection::caret(10)));
        app.flush_project_settings(&root);

        std::fs::write(&file, "ab").unwrap();

        let mut reopened = app_without_gui();
        reopened.open_project(&root, &egui::Context::default());

        assert_eq!(reopened.tabs.len(), 1);
        assert_eq!(
            reopened.tabs[0]
                .buffer
                .text_buffer()
                .selections()
                .primary()
                .head,
            2,
            "offset 10 must clamp to the shrunk file's new length"
        );
    }

    #[test]
    fn resolve_restorable_tab_path_rejects_absolute_and_parent_dir_components() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::write(root.join("f.txt"), "hi").unwrap();

        assert!(IdeApp::resolve_restorable_tab_path(&root, Path::new("f.txt")).is_some());
        assert!(IdeApp::resolve_restorable_tab_path(&root, Path::new("/etc/passwd")).is_none());
        assert!(IdeApp::resolve_restorable_tab_path(&root, Path::new("../f.txt")).is_none());
        assert!(IdeApp::resolve_restorable_tab_path(&root, Path::new("sub/../../f.txt")).is_none());
    }

    #[test]
    fn resolve_restorable_tab_path_rejects_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        assert!(IdeApp::resolve_restorable_tab_path(&root, Path::new("nope.txt")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn resolve_restorable_tab_path_rejects_a_symlink_escaping_the_root() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "shh").unwrap();
        symlink(outside.path().join("secret.txt"), root.join("link.txt")).unwrap();

        assert!(IdeApp::resolve_restorable_tab_path(&root, Path::new("link.txt")).is_none());
    }

    #[test]
    fn flush_project_settings_excludes_untitled_tabs() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_without_gui();
        app.open_project(dir.path(), &egui::Context::default());
        app.new_untitled_tab();

        app.flush_project_settings(dir.path());

        let workspace =
            project_settings::read::<WorkspaceState>(dir.path(), ProjectSettingsFile::Workspace)
                .unwrap()
                .unwrap();
        assert!(workspace.open_tabs.is_empty());
    }

    #[test]
    fn workspace_restore_caps_at_max_restored_tabs() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let total = MAX_RESTORED_TABS + 10;
        let open_tabs: Vec<OpenTabState> = (0..total)
            .map(|i| {
                let name = format!("f{i:03}.txt");
                std::fs::write(root.join(&name), "x").unwrap();
                OpenTabState {
                    path: PathBuf::from(name),
                    cursor_offset: 0,
                }
            })
            .collect();
        project_settings::write(
            &root,
            ProjectSettingsFile::Workspace,
            &WorkspaceState {
                open_tabs,
                active_path: None,
                recent_files: Vec::new(),
            },
        )
        .unwrap();

        let mut app = app_without_gui();
        app.open_project(&root, &egui::Context::default());

        assert_eq!(
            app.tabs.len(),
            MAX_RESTORED_TABS,
            "a crafted workspace.json listing more than MAX_RESTORED_TABS \
             distinct real files must only restore the first MAX_RESTORED_TABS"
        );
    }

    fn go_config() -> LanguageConfig {
        go_config_with_command("definitely-not-a-real-lsp-binary-xyz")
    }

    fn go_config_with_command(command: &str) -> LanguageConfig {
        LanguageConfig {
            name: "Go".to_string(),
            extension: "go".to_string(),
            command: command.to_string(),
            args: Vec::new(),
            extra_extensions: Vec::new(),
        }
    }

    #[test]
    fn load_project_detects_a_custom_language_and_attempts_to_start_its_server() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.go"), "package main").unwrap();
        let mut app = app_without_gui();

        // Set *after* `open_project`: `load_project_settings` (§3.1)
        // overwrites `custom_languages` from the (nonexistent) new
        // project's `.ide/preferences.json` on every project switch, so
        // setting it before `open_project` would just be immediately
        // wiped back to empty. Detection itself only actually runs once
        // `poll_tree_scan` completes below, so setting it here still
        // exercises the same behavior this test is about.
        app.open_project(dir.path(), &egui::Context::default());
        app.custom_languages = vec![go_config()];
        wait_until(|| app.poll_tree_scan());

        assert_eq!(app.active_languages, vec![go_config()]);
        // Command was attempted (deliberately nonexistent) -- distinct from
        // "no language detected", which never touches `lsp` at all.
        assert!(app.lsp.is_running() || app.lsp.server_error.is_some());
    }

    #[test]
    fn load_project_with_no_matching_language_does_not_attempt_to_start_lsp() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("readme.txt"), "hi").unwrap();
        let mut app = app_without_gui();

        app.open_project(dir.path(), &egui::Context::default());
        app.custom_languages = vec![go_config()];
        wait_until(|| app.poll_tree_scan());

        assert_eq!(app.active_languages, Vec::new());
        assert!(!app.lsp.is_running());
        assert!(app.lsp.server_error.is_none());
    }

    #[test]
    fn restore_last_project_with_a_valid_remembered_path_reopens_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_without_gui();

        app.restore_last_project(Some(dir.path().to_path_buf()), &egui::Context::default());

        assert!(app.project.is_some());
        assert!(app.error.is_none());
    }

    #[test]
    fn restore_last_project_with_a_missing_path_leaves_project_none() {
        let mut app = app_without_gui();

        app.restore_last_project(
            Some(std::path::PathBuf::from(
                "/no/such/directory/ide-test-missing",
            )),
            &egui::Context::default(),
        );

        assert!(app.project.is_none());
        assert!(app.error.is_none());
    }

    #[test]
    fn restore_last_project_with_no_remembered_path_is_a_no_op() {
        let mut app = app_without_gui();

        app.restore_last_project(None, &egui::Context::default());

        assert!(app.project.is_none());
        assert!(app.error.is_none());
    }

    #[test]
    fn restart_lsp_uses_the_active_languages_command() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.go"), "package main").unwrap();
        let mut app = app_without_gui();
        app.open_project(dir.path(), &egui::Context::default());
        app.custom_languages = vec![go_config()];
        wait_until(|| app.poll_tree_scan());

        app.restart_lsp();

        assert!(app.lsp.is_running() || app.lsp.server_error.is_some());
    }

    #[test]
    fn run_search_with_no_project_is_a_noop() {
        let mut app = app_without_gui();
        app.search_query = "needle".to_string();
        app.run_search();
        assert!(!app.search.searching);
    }

    #[test]
    fn run_search_with_blank_query_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_without_gui();
        app.open_project(dir.path(), &egui::Context::default());
        app.search_query = "   ".to_string();

        app.run_search();

        assert!(!app.search.searching);
    }

    #[test]
    fn run_search_with_a_project_and_query_starts_a_search() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "needle").unwrap();
        let mut app = app_without_gui();
        app.open_project(dir.path(), &egui::Context::default());
        wait_until(|| app.poll_tree_scan());
        app.search_query = "needle".to_string();

        app.run_search();

        assert!(app.search.searching);
    }

    #[test]
    fn run_search_parses_include_and_exclude_text_at_submit_time_not_per_frame() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "needle").unwrap();
        let mut app = app_without_gui();
        app.open_project(dir.path(), &egui::Context::default());
        wait_until(|| app.poll_tree_scan());
        app.search_query = "needle".to_string();
        // A trailing separator mid-type -- proves parsing only happens once,
        // at `run_search`, not on every keystroke/frame (rev finding, fix
        // round 1): if it were re-derived from the parsed list every frame,
        // this trailing ", " would already have been silently dropped
        // before the user could finish typing the second pattern.
        app.search_include_text = "*.rs, *.toml".to_string();
        app.search_exclude_text = "*.lock, ".to_string();

        app.run_search();

        assert_eq!(
            app.search_options.include,
            vec!["*.rs".to_string(), "*.toml".to_string()]
        );
        assert_eq!(app.search_options.exclude, vec!["*.lock".to_string()]);
        // The raw text itself is left exactly as the user typed it --
        // parsing derives `search_options`, it never mutates the source.
        assert_eq!(app.search_include_text, "*.rs, *.toml");
        assert_eq!(app.search_exclude_text, "*.lock, ");
    }

    #[test]
    fn run_replace_preview_parses_include_and_exclude_text_at_submit_time() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "needle").unwrap();
        let mut app = app_without_gui();
        app.open_project(dir.path(), &egui::Context::default());
        wait_until(|| app.poll_tree_scan());
        app.search_query = "needle".to_string();
        app.search_replacement = "found".to_string();
        app.search_include_text = "*.rs, *.toml".to_string();

        app.run_replace_preview();

        assert_eq!(
            app.search_options.include,
            vec!["*.rs".to_string(), "*.toml".to_string()]
        );
    }

    #[test]
    fn trigger_search_switches_the_bottom_view() {
        let mut app = app_without_gui();
        app.bottom_view = BottomView::Problems;
        app.trigger_search();
        assert_eq!(app.bottom_view, BottomView::Search);
    }

    #[test]
    fn trigger_replace_in_path_reveals_replace_and_switches_to_search_view() {
        let mut app = app_without_gui();
        app.bottom_view = BottomView::Problems;
        app.trigger_replace_in_path();
        assert!(app.search_replace_open);
        assert_eq!(app.bottom_view, BottomView::Search);
    }

    #[test]
    fn trigger_replace_in_path_never_turns_replace_open_back_off() {
        let mut app = app_without_gui();
        app.search_replace_open = true;
        app.trigger_replace_in_path();
        assert!(app.search_replace_open);
    }

    #[test]
    fn run_replace_preview_with_no_project_is_a_noop() {
        let mut app = app_without_gui();
        app.search_query = "needle".to_string();
        app.search_replacement = "found".to_string();
        app.run_replace_preview();
        assert!(!app.search.replacing);
    }

    #[test]
    fn run_replace_preview_with_blank_query_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_without_gui();
        app.open_project(dir.path(), &egui::Context::default());
        app.search_query = "   ".to_string();
        app.search_replacement = "found".to_string();

        app.run_replace_preview();

        assert!(!app.search.replacing);
    }

    #[test]
    fn run_replace_preview_with_blank_replacement_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_without_gui();
        app.open_project(dir.path(), &egui::Context::default());
        app.search_query = "needle".to_string();
        app.search_replacement = String::new();

        app.run_replace_preview();

        assert!(!app.search.replacing);
    }

    #[test]
    fn run_replace_preview_with_a_project_query_and_replacement_starts_a_preview() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "needle").unwrap();
        let mut app = app_without_gui();
        app.open_project(dir.path(), &egui::Context::default());
        wait_until(|| app.poll_tree_scan());
        app.search_query = "needle".to_string();
        app.search_replacement = "found".to_string();

        app.run_replace_preview();

        assert!(app.search.replacing);
    }

    #[test]
    fn open_search_result_opens_the_file_and_sets_pending_cursor_offset() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "hello needle world").unwrap();

        let mut app = app_without_gui();
        app.open_search_result(&file, 6);

        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.pending_cursor_offset, Some(6));
    }

    #[test]
    fn add_custom_language_rejects_empty_fields_after_trimming() {
        let mut app = app_without_gui();
        app.new_language_name = "  ".to_string();
        app.new_language_extension = "go".to_string();
        app.new_language_command = "gopls".to_string();

        app.add_custom_language();

        assert!(app.custom_languages.is_empty());
        assert!(app.language_settings_error.is_some());
    }

    #[test]
    fn add_custom_language_rejects_extension_collision_with_rust() {
        let mut app = app_without_gui();
        app.new_language_name = "Rust-ish".to_string();
        app.new_language_extension = ".RS".to_string();
        app.new_language_command = "some-server".to_string();

        app.add_custom_language();

        assert!(app.custom_languages.is_empty());
        assert!(app.language_settings_error.is_some());
    }

    #[test]
    fn add_custom_language_rejects_extension_collision_with_an_existing_entry() {
        let mut app = app_without_gui();
        app.custom_languages = vec![go_config()];
        app.new_language_name = "Go Again".to_string();
        app.new_language_extension = "GO".to_string();
        app.new_language_command = "gopls2".to_string();

        app.add_custom_language();

        assert_eq!(app.custom_languages.len(), 1);
        assert!(app.language_settings_error.is_some());
    }

    #[test]
    fn add_custom_language_trims_and_strips_a_leading_dot_on_success() {
        let mut app = app_without_gui();
        app.new_language_name = "  Go  ".to_string();
        app.new_language_extension = " .go ".to_string();
        app.new_language_command = " gopls ".to_string();

        app.add_custom_language();

        assert_eq!(
            app.custom_languages,
            vec![LanguageConfig {
                name: "Go".to_string(),
                extension: "go".to_string(),
                command: "gopls".to_string(),
                args: Vec::new(),
                extra_extensions: Vec::new(),
            }]
        );
        assert!(app.language_settings_error.is_none());
        assert!(app.new_language_name.is_empty());
        assert!(app.new_language_extension.is_empty());
        assert!(app.new_language_command.is_empty());
        assert!(app.new_language_args.is_empty());
    }

    #[test]
    fn add_custom_language_splits_args_on_whitespace() {
        let mut app = app_without_gui();
        app.new_language_name = "TypeScript".to_string();
        app.new_language_extension = "ts".to_string();
        app.new_language_command = "typescript-language-server".to_string();
        app.new_language_args = "  --stdio   --log-level=verbose ".to_string();

        app.add_custom_language();

        assert_eq!(
            app.custom_languages,
            vec![LanguageConfig {
                name: "TypeScript".to_string(),
                extension: "ts".to_string(),
                command: "typescript-language-server".to_string(),
                args: vec!["--stdio".to_string(), "--log-level=verbose".to_string()],
                extra_extensions: Vec::new(),
            }]
        );
        assert!(app.new_language_args.is_empty());
    }

    #[test]
    fn add_custom_language_with_a_blank_args_field_parses_to_no_args() {
        let mut app = app_without_gui();
        app.new_language_name = "Go".to_string();
        app.new_language_extension = "go".to_string();
        app.new_language_command = "gopls".to_string();
        app.new_language_args = "   ".to_string();

        app.add_custom_language();

        assert_eq!(app.custom_languages, vec![go_config_with_command("gopls")]);
    }

    #[test]
    fn add_custom_language_while_project_open_takes_effect_immediately() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.go"), "package main").unwrap();
        let mut app = app_without_gui();
        app.open_project(dir.path(), &egui::Context::default());
        wait_until(|| app.poll_tree_scan());
        assert_eq!(app.active_languages, Vec::new());

        app.new_language_name = "Go".to_string();
        app.new_language_extension = "go".to_string();
        app.new_language_command = "definitely-not-a-real-lsp-binary-xyz".to_string();
        app.add_custom_language();

        assert_eq!(app.active_languages, vec![go_config()]);
    }

    #[test]
    fn remove_custom_language_out_of_bounds_is_a_noop() {
        let mut app = app_without_gui();
        app.custom_languages = vec![go_config()];
        app.remove_custom_language(5);
        assert_eq!(app.custom_languages.len(), 1);
    }

    #[test]
    fn remove_custom_language_removes_the_entry_and_redetects() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.go"), "package main").unwrap();
        let mut app = app_without_gui();
        app.open_project(dir.path(), &egui::Context::default());
        app.custom_languages = vec![go_config()];
        wait_until(|| app.poll_tree_scan());
        assert_eq!(app.active_languages, vec![go_config()]);

        app.remove_custom_language(0);

        assert!(app.custom_languages.is_empty());
        assert_eq!(app.active_languages, Vec::new());
        assert!(!app.lsp.is_running());
    }

    fn go_suggestion() -> ide_core::LanguageSuggestion {
        ide_core::LanguageSuggestion {
            marker_file: "go.mod".to_string(),
            config: go_config_with_command("gopls"),
        }
    }

    #[test]
    fn refresh_language_suggestions_with_no_project_is_empty() {
        let mut app = app_without_gui();
        app.refresh_language_suggestions();
        assert!(app.pending_language_suggestions.is_empty());
    }

    #[test]
    fn refresh_language_suggestions_finds_a_go_mod_marker() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("go.mod"), "module example.com/foo").unwrap();
        let mut app = app_without_gui();
        app.open_project(dir.path(), &egui::Context::default());
        wait_until(|| app.poll_tree_scan());

        assert_eq!(app.pending_language_suggestions, vec![go_suggestion()]);
    }

    #[test]
    fn refresh_language_suggestions_still_offered_alongside_an_active_rust_project() {
        // Unlike before `docs/features/multi-language-projects.md`, Rust
        // being active no longer suppresses every other suggestion --
        // several languages can be active in the same project at once
        // now, so a `go.mod` suggestion in a Rust-rooted project must
        // still be offered.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        std::fs::write(dir.path().join("go.mod"), "module example.com/foo").unwrap();
        let mut app = app_without_gui();
        app.open_project(dir.path(), &egui::Context::default());
        wait_until(|| app.poll_tree_scan());

        assert_eq!(
            app.active_languages.first().map(|l| l.name.as_str()),
            Some("Rust")
        );
        assert_eq!(app.pending_language_suggestions, vec![go_suggestion()]);
    }

    #[test]
    fn refresh_language_suggestions_filters_an_already_configured_extension() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("go.mod"), "module example.com/foo").unwrap();
        let mut app = app_without_gui();
        app.open_project(dir.path(), &egui::Context::default());
        app.custom_languages = vec![go_config()];
        wait_until(|| app.poll_tree_scan());

        assert!(app.pending_language_suggestions.is_empty());
    }

    #[test]
    fn refresh_language_suggestions_filters_a_previously_dismissed_marker() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("go.mod"), "module example.com/foo").unwrap();
        let mut app = app_without_gui();
        app.open_project(dir.path(), &egui::Context::default());
        app.dismissed_language_suggestions = vec!["go.mod".to_string()];
        wait_until(|| app.poll_tree_scan());

        assert!(app.pending_language_suggestions.is_empty());
    }

    #[test]
    fn enable_language_suggestion_adds_the_config_and_redetects() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("go.mod"), "module example.com/foo").unwrap();
        // `detect_language`'s custom-config matching (unlike the marker
        // suggestion itself) is tree-wide-extension-based, not
        // marker-based -- a real `.go` file must exist for `redetect_
        // language` to actually pick the newly-enabled config back up.
        std::fs::write(dir.path().join("main.go"), "package main").unwrap();
        let mut app = app_without_gui();
        app.open_project(dir.path(), &egui::Context::default());
        wait_until(|| app.poll_tree_scan());
        assert_eq!(app.pending_language_suggestions, vec![go_suggestion()]);

        app.enable_language_suggestion(go_suggestion());

        assert_eq!(app.custom_languages, vec![go_config_with_command("gopls")]);
        assert!(app.pending_language_suggestions.is_empty());
        assert_eq!(app.active_languages, vec![go_config_with_command("gopls")]);
    }

    #[test]
    fn dismiss_language_suggestion_records_it_without_touching_custom_languages() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("go.mod"), "module example.com/foo").unwrap();
        let mut app = app_without_gui();
        app.open_project(dir.path(), &egui::Context::default());
        wait_until(|| app.poll_tree_scan());

        app.dismiss_language_suggestion(go_suggestion());

        assert!(app.custom_languages.is_empty());
        assert!(app.pending_language_suggestions.is_empty());
        assert_eq!(
            app.dismissed_language_suggestions,
            vec!["go.mod".to_string()]
        );
    }

    #[test]
    fn dismiss_language_suggestion_does_not_duplicate_on_a_second_call() {
        let mut app = app_without_gui();
        app.dismiss_language_suggestion(go_suggestion());
        app.dismiss_language_suggestion(go_suggestion());

        assert_eq!(
            app.dismissed_language_suggestions,
            vec!["go.mod".to_string()]
        );
    }

    #[test]
    fn open_file_sends_did_open_and_close_tab_sends_did_close_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        let mut app = app_without_gui();
        // No client running (no project opened) -- `LspBridge::send` must
        // be a silent no-op rather than panicking, exercised through the
        // real open_file/close_tab_now call sites.
        app.open_file(&file);
        assert_eq!(app.tabs.len(), 1);
        app.close_tab_now(0);
        assert!(app.tabs.is_empty());
    }

    #[test]
    fn notify_lsp_changed_sends_the_current_text() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);

        // No client is running in a headless test: must not panic either
        // before or after an edit.
        app.notify_lsp_changed(0);
        app.tabs[0].buffer.insert(12, " // edited");
        app.notify_lsp_changed(0);
        assert_eq!(app.tabs[0].buffer.text(), "fn main() {} // edited");
    }

    #[test]
    fn sync_tab_diagnostics_copies_from_the_workspace_map() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        // An untitled tab has no path, so nothing to key the map by --
        // must not panic, and diagnostics stay empty.
        app.sync_tab_diagnostics();
        assert!(app.tabs[0].diagnostics.is_empty());

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        // `open_file` canonicalizes internally (`file-watcher.md` §3.4's
        // "Path identity" invariant); key the map with the same canonical
        // form `sync_tab_diagnostics` will look up against.
        let file = file.canonicalize().unwrap();
        app.open_file(&file);
        app.lsp.diagnostics.insert(
            file.clone(),
            vec![ide_lsp::Diagnostic {
                range: ide_lsp::Range {
                    start: Position {
                        line: 0,
                        character: 0,
                    },
                    end: Position {
                        line: 0,
                        character: 1,
                    },
                },
                severity: ide_lsp::DiagnosticSeverity::Error,
                message: "boom".to_string(),
            }],
        );

        app.sync_tab_diagnostics();
        assert_eq!(app.tabs[1].diagnostics.len(), 1);
        assert_eq!(app.tabs[1].diagnostics[0].message, "boom");
    }

    #[test]
    fn open_diagnostic_opens_the_file_and_sets_pending_cursor_offset() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        let mut app = app_without_gui();
        app.open_diagnostic(
            &file,
            Position {
                line: 0,
                character: 3,
            },
        );

        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.pending_cursor_offset, Some(3));
    }

    #[test]
    fn open_usage_opens_the_file_and_sets_pending_cursor_offset() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        let mut app = app_without_gui();
        app.open_usage(
            &file,
            Position {
                line: 0,
                character: 3,
            },
        );

        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.pending_cursor_offset, Some(3));
    }

    // `find_usages_target`'s no-op conditions are tested directly here
    // (rather than through `find_usages` + observing `LspBridge` state)
    // because `LspBridge::find_references` itself no-ops whenever no
    // client is running -- true throughout this test harness -- which
    // would make every one of these conditions indistinguishable from
    // each other (or from correctly-reached-but-unanswerable) if only
    // observed via `finding_references`/`references`.

    #[test]
    fn find_usages_target_with_no_active_tab_is_none() {
        let mut app = app_without_gui();
        app.active_cursor_offset = Some(0);
        assert_eq!(app.find_usages_target(), None);
    }

    #[test]
    fn find_usages_target_with_source_control_view_is_none_even_with_a_stale_offset() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        // Simulate a cursor offset left over from a prior frame where the
        // editor was showing, then a switch away from it -- `Alt+F7` firing
        // here must not use this stale offset (doc §2.2).
        app.active_cursor_offset = Some(3);
        app.view_mode = ViewMode::SourceControl;

        assert_eq!(app.find_usages_target(), None);
    }

    #[test]
    fn find_usages_target_with_no_cursor_offset_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.active_cursor_offset = None;

        assert_eq!(app.find_usages_target(), None);
    }

    #[test]
    fn find_usages_target_with_untitled_tab_is_none() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.active_cursor_offset = Some(0);

        assert_eq!(app.find_usages_target(), None);
    }

    #[test]
    fn find_usages_target_with_a_valid_cursor_offset_computes_the_position() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        // `open_file` canonicalizes internally; compare against the same
        // canonical form it will have stored on the tab's buffer.
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.active_cursor_offset = Some(3);

        assert_eq!(
            app.find_usages_target(),
            Some((
                file,
                Position {
                    line: 0,
                    character: 3
                }
            ))
        );
    }

    #[test]
    fn find_usages_with_no_running_lsp_client_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.active_cursor_offset = Some(3);
        app.find_usages();

        // No LSP client is running in this test harness -- `find_usages`
        // must not panic when `LspBridge::find_references` itself no-ops.
        assert!(!app.lsp.finding_references);
    }

    #[test]
    fn trigger_find_usages_switches_the_bottom_view() {
        let mut app = app_without_gui();
        app.bottom_view = BottomView::Problems;
        app.trigger_find_usages();
        assert_eq!(app.bottom_view, BottomView::Usages);
    }

    // ---- Usages popup ----

    #[test]
    fn trigger_find_usages_popup_opens_the_window_without_touching_the_bottom_view() {
        let mut app = app_without_gui();
        app.bottom_view = BottomView::Problems;

        app.trigger_find_usages_popup();

        assert!(app.show_usages_popup);
        assert_eq!(app.bottom_view, BottomView::Problems);
    }

    #[test]
    fn escape_belongs_to_the_editor_only_while_several_cursors_are_up() {
        use ide_core::{Selection, Selections};

        let mut app = app_without_gui();
        app.tabs.push(Tab::untitled("Untitled".to_string()));
        app.active_tab = Some(0);
        app.tabs[0].buffer.insert(0, "one\ntwo");

        // One cursor: the popup keeps its own Escape.
        assert!(!app.editor_owns_escape());

        app.tabs[0]
            .buffer
            .text_buffer_mut()
            .set_selections(Selections::new(
                vec![Selection::caret(0), Selection::caret(4)],
                0,
            ));
        assert!(app.editor_owns_escape());

        // Not while the editor isn't even the visible view...
        app.view_mode = ViewMode::SourceControl;
        assert!(!app.editor_owns_escape());
        app.view_mode = ViewMode::Editor;

        // ...and not without a tab.
        app.active_tab = None;
        assert!(!app.editor_owns_escape());
    }

    #[test]
    fn sorted_references_orders_by_path_then_position() {
        fn loc(path: &str, line: u32, character: u32) -> Location {
            Location {
                path: PathBuf::from(path),
                range: ide_lsp::Range {
                    start: Position { line, character },
                    end: Position {
                        line,
                        character: character + 1,
                    },
                },
            }
        }

        let mut app = app_without_gui();
        app.lsp.references = vec![
            loc("/p/b.rs", 1, 0),
            loc("/p/a.rs", 5, 9),
            loc("/p/a.rs", 5, 2),
            loc("/p/a.rs", 1, 0),
        ];

        let sorted: Vec<(String, u32, u32)> = app
            .sorted_references()
            .into_iter()
            .map(|l| {
                (
                    l.path.display().to_string(),
                    l.range.start.line,
                    l.range.start.character,
                )
            })
            .collect();
        assert_eq!(
            sorted,
            vec![
                ("/p/a.rs".to_string(), 1, 0),
                ("/p/a.rs".to_string(), 5, 2),
                ("/p/a.rs".to_string(), 5, 9),
                ("/p/b.rs".to_string(), 1, 0),
            ]
        );
    }

    #[test]
    fn display_path_is_relative_to_the_project_root() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_without_gui();
        app.open_project(dir.path(), &egui::Context::default());
        // Built from the project's own root rather than `dir.path()`:
        // `Project::open` canonicalizes, which on macOS rewrites /var into
        // /private/var.
        let file = app
            .project
            .as_ref()
            .unwrap()
            .root()
            .join("src")
            .join("main.rs");

        assert_eq!(app.display_path(&file), "src/main.rs");
        // Outside the root (and with no project at all) it stays absolute.
        assert_eq!(
            app.display_path(Path::new("/elsewhere/x.rs")),
            "/elsewhere/x.rs"
        );
    }

    // ---- C1: goto-definition ----

    #[test]
    fn open_definition_opens_the_file_and_sets_pending_cursor_offset() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        let mut app = app_without_gui();
        app.open_definition(
            &file,
            Position {
                line: 0,
                character: 3,
            },
        );

        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.pending_cursor_offset, Some(3));
    }

    #[test]
    fn trigger_go_to_declaration_sets_goto_action_and_clears_a_stale_popup() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.active_cursor_offset = Some(3);
        app.show_goto_popup = true;

        app.trigger_go_to_declaration();

        assert_eq!(app.goto_action, Some(GotoKind::Definition));
        assert!(!app.show_goto_popup);
        // No LSP client is running in this test harness -- must not panic
        // when `LspBridge::go_to_definition` itself no-ops.
        assert!(!app.lsp.finding_goto);
    }

    #[test]
    fn trigger_go_to_type_declaration_sets_the_type_definition_kind() {
        let mut app = app_without_gui();
        app.trigger_go_to_type_declaration();
        assert_eq!(app.goto_action, Some(GotoKind::TypeDefinition));
    }

    #[test]
    fn trigger_go_to_implementation_sets_the_implementation_kind() {
        let mut app = app_without_gui();
        app.trigger_go_to_implementation();
        assert_eq!(app.goto_action, Some(GotoKind::Implementation));
    }

    #[test]
    fn trigger_go_to_declaration_with_no_active_tab_still_sets_goto_action() {
        // Shares `find_usages_target`'s no-op gating (doc §3.1) -- the
        // query itself no-ops with no active tab, but `goto_action` is set
        // unconditionally before that check runs, same as `show_usages_popup`
        // is unconditionally opened by `trigger_find_usages_popup`.
        let mut app = app_without_gui();
        app.trigger_go_to_declaration();
        assert_eq!(app.goto_action, Some(GotoKind::Definition));
        assert!(!app.lsp.finding_goto);
    }

    #[test]
    fn handle_goto_response_is_a_noop_when_not_ready() {
        let mut app = app_without_gui();
        app.lsp.goto_ready = false;
        app.lsp.goto = vec![Location {
            path: PathBuf::from("/x.rs"),
            range: ide_lsp::Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 1,
                },
            },
        }];
        app.handle_goto_response();
        // Not consumed -- no tab was opened, no popup shown.
        assert!(app.tabs.is_empty());
        assert!(!app.show_goto_popup);
    }

    #[test]
    fn handle_goto_response_with_exactly_one_result_jumps_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        app.lsp.goto_ready = true;
        app.lsp.goto = vec![Location {
            path: file.clone(),
            range: ide_lsp::Range {
                start: Position {
                    line: 0,
                    character: 3,
                },
                end: Position {
                    line: 0,
                    character: 7,
                },
            },
        }];

        app.handle_goto_response();

        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.tabs[0].buffer.path(), Some(file.as_path()));
        assert_eq!(app.pending_cursor_offset, Some(3));
        assert!(!app.show_goto_popup);
    }

    #[test]
    fn handle_goto_response_with_zero_results_opens_the_popup() {
        let mut app = app_without_gui();
        app.lsp.goto_ready = true;
        app.lsp.goto = Vec::new();

        app.handle_goto_response();

        assert!(app.show_goto_popup);
        assert!(app.tabs.is_empty());
    }

    #[test]
    fn handle_goto_response_with_multiple_results_opens_the_popup() {
        fn loc(path: &str) -> Location {
            Location {
                path: PathBuf::from(path),
                range: ide_lsp::Range {
                    start: Position {
                        line: 0,
                        character: 0,
                    },
                    end: Position {
                        line: 0,
                        character: 1,
                    },
                },
            }
        }

        let mut app = app_without_gui();
        app.lsp.goto_ready = true;
        app.lsp.goto = vec![loc("/a.rs"), loc("/b.rs")];

        app.handle_goto_response();

        assert!(app.show_goto_popup);
        assert!(app.tabs.is_empty());
    }

    #[test]
    fn sorted_goto_orders_by_path_then_position() {
        fn loc(path: &str, line: u32, character: u32) -> Location {
            Location {
                path: PathBuf::from(path),
                range: ide_lsp::Range {
                    start: Position { line, character },
                    end: Position {
                        line,
                        character: character + 1,
                    },
                },
            }
        }

        let mut app = app_without_gui();
        app.lsp.goto = vec![
            loc("/p/b.rs", 1, 0),
            loc("/p/a.rs", 5, 9),
            loc("/p/a.rs", 5, 2),
        ];

        let sorted: Vec<(String, u32, u32)> = app
            .sorted_goto()
            .into_iter()
            .map(|l| {
                (
                    l.path.display().to_string(),
                    l.range.start.line,
                    l.range.start.character,
                )
            })
            .collect();
        assert_eq!(
            sorted,
            vec![
                ("/p/a.rs".to_string(), 5, 2),
                ("/p/a.rs".to_string(), 5, 9),
                ("/p/b.rs".to_string(), 1, 0),
            ]
        );
    }

    #[test]
    fn goto_action_label_covers_all_three_kinds_and_the_default() {
        let mut app = app_without_gui();
        assert_eq!(app.goto_action_label(), "Declaration");
        app.goto_action = Some(GotoKind::Definition);
        assert_eq!(app.goto_action_label(), "Declaration");
        app.goto_action = Some(GotoKind::TypeDefinition);
        assert_eq!(app.goto_action_label(), "Type Declaration");
        app.goto_action = Some(GotoKind::Implementation);
        assert_eq!(app.goto_action_label(), "Implementation");
    }

    // ---- goto-declaration-interface-redirect ----

    fn symbol(name: &str, kind: SymbolKind, path: &Path, start: u32, end: u32) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind,
            container_name: None,
            location: Location {
                path: path.to_path_buf(),
                range: ide_lsp::Range {
                    start: Position {
                        line: start,
                        character: 0,
                    },
                    end: Position {
                        line: end,
                        character: 0,
                    },
                },
            },
        }
    }

    #[test]
    fn trigger_go_to_declaration_records_the_origin_for_the_redirect_check() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.active_cursor_offset = Some(3);

        app.trigger_go_to_declaration();

        let (path, position) = app.goto_declaration_origin.clone().unwrap();
        assert_eq!(path, file.canonicalize().unwrap());
        assert_eq!(position.character, 3);
    }

    #[test]
    fn trigger_go_to_declaration_with_no_target_clears_the_origin() {
        let mut app = app_without_gui();
        app.goto_declaration_origin = Some((
            PathBuf::from("/stale.rs"),
            Position {
                line: 0,
                character: 0,
            },
        ));

        app.trigger_go_to_declaration();

        assert!(app.goto_declaration_origin.is_none());
    }

    #[test]
    fn handle_goto_response_with_a_single_definition_result_defers_to_the_interface_check() {
        let file = PathBuf::from("/f.rs");
        let mut app = app_without_gui();
        app.goto_action = Some(GotoKind::Definition);
        app.lsp.goto_ready = true;
        app.lsp.goto = vec![Location {
            path: file.clone(),
            range: ide_lsp::Range {
                start: Position {
                    line: 0,
                    character: 3,
                },
                end: Position {
                    line: 0,
                    character: 7,
                },
            },
        }];

        app.handle_goto_response();

        // Deferred, not jumped -- no tab opened yet, no popup either.
        assert!(app.tabs.is_empty());
        assert!(!app.show_goto_popup);
        assert_eq!(app.pending_interface_check.as_ref().unwrap().path, file);
    }

    #[test]
    fn handle_goto_response_for_type_definition_jumps_immediately_unaffected_by_the_redirect() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        app.goto_action = Some(GotoKind::TypeDefinition);
        app.lsp.goto_ready = true;
        app.lsp.goto = vec![Location {
            path: file.clone(),
            range: ide_lsp::Range {
                start: Position {
                    line: 0,
                    character: 3,
                },
                end: Position {
                    line: 0,
                    character: 7,
                },
            },
        }];

        app.handle_goto_response();

        assert_eq!(app.tabs.len(), 1);
        assert!(app.pending_interface_check.is_none());
    }

    #[test]
    fn handle_interface_check_response_is_a_noop_when_not_ready() {
        let mut app = app_without_gui();
        app.lsp.document_symbols_ready = false;
        app.pending_interface_check = Some(Location {
            path: PathBuf::from("/f.rs"),
            range: ide_lsp::Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 0,
                },
            },
        });
        app.handle_interface_check_response();
        assert!(app.pending_interface_check.is_some());
        assert!(app.tabs.is_empty());
    }

    #[test]
    fn handle_interface_check_response_ignores_a_response_for_a_different_file() {
        let mut app = app_without_gui();
        app.lsp.document_symbols_ready = true;
        app.lsp.document_symbols_path = Some(PathBuf::from("/other.rs"));
        app.pending_interface_check = Some(Location {
            path: PathBuf::from("/f.rs"),
            range: ide_lsp::Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 0,
                },
            },
        });

        app.handle_interface_check_response();

        // Left outstanding -- this response belongs to some other query.
        assert!(app.pending_interface_check.is_some());
        assert!(app.tabs.is_empty());
    }

    #[test]
    fn handle_interface_check_response_jumps_directly_on_a_plain_function() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn helper() {}").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        app.goto_action = Some(GotoKind::Definition);
        app.goto_declaration_origin = Some((
            file.clone(),
            Position {
                line: 5,
                character: 5,
            },
        ));
        app.pending_interface_check = Some(Location {
            path: file.clone(),
            range: ide_lsp::Range {
                start: Position {
                    line: 0,
                    character: 3,
                },
                end: Position {
                    line: 0,
                    character: 9,
                },
            },
        });
        app.lsp.document_symbols_ready = true;
        app.lsp.document_symbols_path = Some(file.clone());
        app.lsp.document_symbols = vec![symbol("helper", SymbolKind::Function, &file, 0, 0)];

        app.handle_interface_check_response();

        assert!(app.pending_interface_check.is_none());
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.tabs[0].buffer.path(), Some(file.as_path()));
        assert_eq!(app.pending_cursor_offset, Some(3));
        // Not redirected -- goto_action stays Definition.
        assert_eq!(app.goto_action, Some(GotoKind::Definition));
    }

    #[test]
    fn handle_interface_check_response_redirects_to_implementation_on_an_interface() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "trait Logger { fn log(&self); }").unwrap();
        let file = file.canonicalize().unwrap();

        let origin_position = Position {
            line: 9,
            character: 1,
        };
        let mut app = app_without_gui();
        app.goto_action = Some(GotoKind::Definition);
        app.goto_declaration_origin = Some((file.clone(), origin_position));
        app.pending_interface_check = Some(Location {
            path: file.clone(),
            range: ide_lsp::Range {
                start: Position {
                    line: 0,
                    character: 6,
                },
                end: Position {
                    line: 0,
                    character: 12,
                },
            },
        });
        app.lsp.document_symbols_ready = true;
        app.lsp.document_symbols_path = Some(file.clone());
        app.lsp.document_symbols = vec![symbol("Logger", SymbolKind::Interface, &file, 0, 1)];

        app.handle_interface_check_response();

        assert!(app.pending_interface_check.is_none());
        // Redirected -- no direct jump, goto_action switched to
        // Implementation, and no tab opened by this call (the redirected
        // query's own response, not exercised here, is what would open one).
        assert!(app.tabs.is_empty());
        assert_eq!(app.goto_action, Some(GotoKind::Implementation));
    }

    #[test]
    fn handle_interface_check_response_on_an_interface_with_no_recorded_origin_falls_back_to_the_location(
    ) {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "trait Logger { fn log(&self); }").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        app.goto_action = Some(GotoKind::Definition);
        app.goto_declaration_origin = None;
        app.pending_interface_check = Some(Location {
            path: file.clone(),
            range: ide_lsp::Range {
                start: Position {
                    line: 0,
                    character: 6,
                },
                end: Position {
                    line: 0,
                    character: 12,
                },
            },
        });
        app.lsp.document_symbols_ready = true;
        app.lsp.document_symbols_path = Some(file.clone());
        app.lsp.document_symbols = vec![symbol("Logger", SymbolKind::Interface, &file, 0, 1)];

        app.handle_interface_check_response();

        assert!(app.pending_interface_check.is_none());
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.tabs[0].buffer.path(), Some(file.as_path()));
        assert_eq!(app.pending_cursor_offset, Some(6));
    }

    #[test]
    fn trigger_quick_documentation_opens_the_popup_even_with_no_active_tab() {
        // Mirrors `trigger_go_to_declaration_with_no_active_tab_still_sets_
        // goto_action` -- `show_hover_popup` is set unconditionally before
        // `find_usages_target`'s no-active-tab gating runs.
        let mut app = app_without_gui();
        app.trigger_quick_documentation();
        assert!(app.show_hover_popup);
        assert!(!app.lsp.finding_hover);
    }

    #[test]
    fn trigger_quick_documentation_with_a_valid_target_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.active_cursor_offset = Some(3);

        app.trigger_quick_documentation();

        assert!(app.show_hover_popup);
        // No LSP client is running in this test harness -- must not panic
        // when `LspBridge::request_hover` itself no-ops.
        assert!(!app.lsp.finding_hover);
    }

    #[test]
    fn sync_document_highlights_with_no_target_clears_the_last_highlighted_target() {
        let mut app = app_without_gui();
        app.last_highlighted_target = Some((
            PathBuf::from("/x.rs"),
            Position {
                line: 0,
                character: 0,
            },
        ));

        app.sync_document_highlights();

        assert_eq!(app.last_highlighted_target, None);
    }

    #[test]
    fn sync_document_highlights_with_a_new_target_records_it() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.active_cursor_offset = Some(3);

        app.sync_document_highlights();

        assert_eq!(
            app.last_highlighted_target,
            Some((
                file,
                Position {
                    line: 0,
                    character: 3,
                }
            ))
        );
    }

    #[test]
    fn sync_document_highlights_with_the_same_target_twice_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.active_cursor_offset = Some(3);

        app.sync_document_highlights();
        let first = app.last_highlighted_target.clone();
        app.sync_document_highlights();

        assert_eq!(app.last_highlighted_target, first);
    }

    #[test]
    fn sync_inlay_hints_with_an_untitled_tab_does_not_panic() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.sync_inlay_hints(0);
    }

    #[test]
    fn sync_inlay_hints_with_a_real_path_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);

        // No LSP client is running in this test harness -- must not panic
        // when `LspBridge::request_inlay_hints` itself no-ops.
        app.sync_inlay_hints(0);
    }

    #[test]
    fn active_inlay_hints_with_no_active_tab_is_empty() {
        let app = app_without_gui();
        assert!(app.active_inlay_hints().is_empty());
    }

    #[test]
    fn active_inlay_hints_with_an_untitled_tab_is_empty() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        assert!(app.active_inlay_hints().is_empty());
    }

    #[test]
    fn active_inlay_hints_reads_from_the_workspace_map_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.lsp.inlay_hints.insert(
            file.clone(),
            vec![InlayHint {
                position: Position {
                    line: 0,
                    character: 3,
                },
                label: ": i32".to_string(),
                padding_left: true,
                padding_right: false,
            }],
        );

        let hints = app.active_inlay_hints();
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].label, ": i32");
    }

    #[test]
    fn poll_menu_events_with_no_native_menu_ever_installed_is_a_noop() {
        // `app_without_gui` builds `IdeApp` via a direct struct literal,
        // bypassing `IdeApp::new` -- so `menu::install_native_menu` (which
        // would attach a real macOS menu) never ran in this process, and
        // `muda::MenuEvent::receiver()` is just an always-empty channel.
        let mut app = app_without_gui();
        assert!(!app.poll_menu_events(&egui::Context::default()));
    }

    #[test]
    fn sync_semantic_tokens_with_an_untitled_tab_does_not_panic() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.sync_semantic_tokens(0);
    }

    #[test]
    fn sync_semantic_tokens_with_a_real_path_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);

        // No LSP client is running in this test harness -- must not panic
        // when `LspBridge::request_semantic_tokens` itself no-ops.
        app.sync_semantic_tokens(0);
    }

    #[test]
    fn active_semantic_tokens_with_no_active_tab_is_empty() {
        let app = app_without_gui();
        assert!(app.active_semantic_tokens().is_empty());
    }

    #[test]
    fn active_semantic_tokens_with_an_untitled_tab_is_empty() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        assert!(app.active_semantic_tokens().is_empty());
    }

    #[test]
    fn active_semantic_tokens_reads_from_the_workspace_map_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.lsp.semantic_tokens.insert(
            file.clone(),
            vec![ide_lsp::SemanticToken {
                position: Position {
                    line: 0,
                    character: 3,
                },
                length: 4,
                kind: ide_lsp::SemanticTokenKind::Function,
            }],
        );

        let tokens = app.active_semantic_tokens();
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].kind, ide_lsp::SemanticTokenKind::Function);
    }

    #[test]
    fn sync_code_actions_with_no_target_clears_the_last_code_actions_target() {
        let mut app = app_without_gui();
        app.last_code_actions_target = Some((
            PathBuf::from("/x.rs"),
            Position {
                line: 0,
                character: 0,
            },
        ));

        app.sync_code_actions();

        assert_eq!(app.last_code_actions_target, None);
    }

    #[test]
    fn sync_code_actions_with_a_new_target_records_it() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.active_cursor_offset = Some(3);

        app.sync_code_actions();

        assert_eq!(
            app.last_code_actions_target,
            Some((
                file,
                Position {
                    line: 0,
                    character: 3,
                }
            ))
        );
    }

    #[test]
    fn sync_code_actions_with_the_same_target_twice_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.active_cursor_offset = Some(3);

        app.sync_code_actions();
        let first = app.last_code_actions_target.clone();
        app.sync_code_actions();

        assert_eq!(app.last_code_actions_target, first);
    }

    #[test]
    fn trigger_show_intention_actions_opens_the_popup_without_sending_a_request() {
        // doc §2.3: unlike `trigger_quick_documentation`, this must not
        // touch `lsp.code_actions`/`code_actions_target` at all -- it only
        // opens the popup on whatever `sync_code_actions` already fetched.
        let mut app = app_without_gui();
        app.lsp.code_actions = vec![CodeAction {
            index: 0,
            title: "Import `Foo`".to_string(),
            kind: Some("quickfix".to_string()),
            is_preferred: true,
            disabled_reason: None,
        }];

        app.trigger_show_intention_actions();

        assert!(app.show_code_actions_popup);
        assert_eq!(app.lsp.code_actions.len(), 1);
    }

    #[test]
    fn select_code_action_closes_the_popup_and_does_not_panic_without_a_client() {
        let mut app = app_without_gui();
        app.show_code_actions_popup = true;

        app.select_code_action(0);

        assert!(!app.show_code_actions_popup);
    }

    #[test]
    fn code_action_gutter_line_is_none_with_no_code_actions() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.active_cursor_offset = Some(3);

        assert_eq!(app.code_action_gutter_line(), None);
    }

    #[test]
    fn code_action_gutter_line_is_none_with_no_active_tab() {
        let mut app = app_without_gui();
        app.lsp.code_actions = vec![CodeAction {
            index: 0,
            title: "x".to_string(),
            kind: None,
            is_preferred: false,
            disabled_reason: None,
        }];

        assert_eq!(app.code_action_gutter_line(), None);
    }

    #[test]
    fn code_action_gutter_line_is_none_when_the_target_is_a_different_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.lsp.code_actions = vec![CodeAction {
            index: 0,
            title: "x".to_string(),
            kind: None,
            is_preferred: false,
            disabled_reason: None,
        }];
        app.lsp.code_actions_target = Some((
            PathBuf::from("/other.rs"),
            Position {
                line: 0,
                character: 0,
            },
        ));

        assert_eq!(app.code_action_gutter_line(), None);
    }

    #[test]
    fn code_action_gutter_line_resolves_the_targets_line() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}\nlet x = 1;").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.lsp.code_actions = vec![CodeAction {
            index: 0,
            title: "x".to_string(),
            kind: None,
            is_preferred: false,
            disabled_reason: None,
        }];
        app.lsp.code_actions_target = Some((
            file,
            Position {
                line: 1,
                character: 0,
            },
        ));

        assert_eq!(app.code_action_gutter_line(), Some(1));
    }

    #[test]
    fn handle_workspace_edit_ready_is_a_noop_when_not_ready() {
        let mut app = app_without_gui();
        app.lsp.workspace_edit_ready = false;
        app.lsp.workspace_edit = Some(ide_lsp::WorkspaceEdit { edits: vec![] });

        app.handle_workspace_edit_ready();

        assert_eq!(app.error, None);
        assert!(app.lsp.workspace_edit.is_some());
    }

    #[test]
    fn handle_workspace_edit_ready_with_no_edit_reports_nothing_to_apply() {
        let mut app = app_without_gui();
        app.lsp.workspace_edit_ready = true;
        app.lsp.workspace_edit = None;
        app.lsp.workspace_edit_label = Some("Organize imports".to_string());

        app.handle_workspace_edit_ready();

        assert_eq!(
            app.error,
            Some("Organize imports: nothing to apply".to_string())
        );
    }

    #[test]
    fn handle_workspace_edit_ready_applies_to_disk_for_a_file_with_no_open_tab() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        app.lsp.workspace_edit_ready = true;
        app.lsp.workspace_edit_label = Some("Import `Foo`".to_string());
        app.lsp.workspace_edit = Some(ide_lsp::WorkspaceEdit {
            edits: vec![ide_lsp::FileEdit {
                path: file.clone(),
                text_edits: vec![ide_lsp::TextEdit {
                    range: ide_lsp::Range {
                        start: Position {
                            line: 0,
                            character: 0,
                        },
                        end: Position {
                            line: 0,
                            character: 0,
                        },
                    },
                    new_text: "// hi\n".to_string(),
                }],
            }],
        });

        app.handle_workspace_edit_ready();

        assert_eq!(
            app.error,
            Some("Import `Foo`: applied to 1 file".to_string())
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "// hi\nfn main() {}"
        );
    }

    #[test]
    fn handle_workspace_edit_ready_applies_to_an_open_tabs_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.lsp.workspace_edit_ready = true;
        app.lsp.workspace_edit_label = Some("Import `Foo`".to_string());
        app.lsp.workspace_edit = Some(ide_lsp::WorkspaceEdit {
            edits: vec![ide_lsp::FileEdit {
                path: file.clone(),
                text_edits: vec![ide_lsp::TextEdit {
                    range: ide_lsp::Range {
                        start: Position {
                            line: 0,
                            character: 0,
                        },
                        end: Position {
                            line: 0,
                            character: 0,
                        },
                    },
                    new_text: "// hi\n".to_string(),
                }],
            }],
        });

        app.handle_workspace_edit_ready();

        assert_eq!(
            app.error,
            Some("Import `Foo`: applied to 1 file".to_string())
        );
        assert_eq!(app.tabs[0].buffer.text(), "// hi\nfn main() {}");
        // The disk copy is untouched -- this file had an open tab, so the
        // edit went through the buffer, not `apply_workspace_edit_to_disk`.
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "fn main() {}");
    }

    #[test]
    fn handle_workspace_edit_ready_with_an_unreadable_file_reports_the_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.rs");

        let mut app = app_without_gui();
        app.lsp.workspace_edit_ready = true;
        app.lsp.workspace_edit_label = Some("Fix".to_string());
        app.lsp.workspace_edit = Some(ide_lsp::WorkspaceEdit {
            edits: vec![ide_lsp::FileEdit {
                path: missing.clone(),
                text_edits: vec![ide_lsp::TextEdit {
                    range: ide_lsp::Range {
                        start: Position {
                            line: 0,
                            character: 0,
                        },
                        end: Position {
                            line: 0,
                            character: 0,
                        },
                    },
                    new_text: "x".to_string(),
                }],
            }],
        });

        app.handle_workspace_edit_ready();

        let err = app.error.expect("an error should be reported");
        assert!(err.starts_with("Fix: could not read"));
    }

    #[test]
    fn handle_workspace_edit_ready_with_an_out_of_range_edit_reports_it_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        app.lsp.workspace_edit_ready = true;
        app.lsp.workspace_edit_label = Some("Fix".to_string());
        app.lsp.workspace_edit = Some(ide_lsp::WorkspaceEdit {
            edits: vec![ide_lsp::FileEdit {
                path: file.clone(),
                text_edits: vec![ide_lsp::TextEdit {
                    range: ide_lsp::Range {
                        start: Position {
                            line: 99,
                            character: 0,
                        },
                        end: Position {
                            line: 99,
                            character: 1,
                        },
                    },
                    new_text: "x".to_string(),
                }],
            }],
        });

        app.handle_workspace_edit_ready();

        let err = app.error.expect("an error should be reported");
        assert!(err.starts_with("Fix: an edit for"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "fn main() {}");
    }

    // ---- D2: refactor this ----

    fn refactor_action(index: usize, title: &str, kind: &str) -> CodeAction {
        CodeAction {
            index,
            title: title.to_string(),
            kind: Some(kind.to_string()),
            is_preferred: false,
            disabled_reason: None,
        }
    }

    #[test]
    fn trigger_refactor_this_with_no_refactor_kind_action_sets_an_error() {
        let mut app = app_without_gui();
        app.lsp.code_actions = vec![refactor_action(0, "Import `Foo`", "quickfix")];

        app.trigger_refactor_this();

        assert!(!app.show_refactor_menu_popup);
        assert_eq!(
            app.error,
            Some("Refactor This: no refactoring available here".to_string())
        );
    }

    #[test]
    fn trigger_refactor_this_opens_the_popup_without_sending_a_request() {
        let mut app = app_without_gui();
        app.lsp.code_actions = vec![
            refactor_action(0, "Import `Foo`", "quickfix"),
            refactor_action(1, "Extract into variable", "refactor.extract"),
        ];

        app.trigger_refactor_this();

        assert!(app.show_refactor_menu_popup);
        assert_eq!(app.lsp.code_actions.len(), 2);
    }

    #[test]
    fn select_refactor_action_closes_the_popup_and_sets_via_preview() {
        let mut app = app_without_gui();
        app.show_refactor_menu_popup = true;

        app.select_refactor_action(0);

        assert!(!app.show_refactor_menu_popup);
        assert!(app.via_refactor_preview);
    }

    #[test]
    fn trigger_direct_refactor_extract_variable_matches_kind_and_title() {
        let mut app = app_without_gui();
        app.lsp.code_actions = vec![
            refactor_action(0, "Extract into function", "refactor.extract"),
            refactor_action(1, "Extract into variable", "refactor.extract"),
        ];

        app.trigger_direct_refactor(DirectRefactorKind::ExtractVariable);

        assert!(app.via_refactor_preview);
        assert!(app.error.is_none());
    }

    #[test]
    fn trigger_direct_refactor_skips_a_disabled_action() {
        let mut app = app_without_gui();
        app.lsp.code_actions = vec![CodeAction {
            index: 0,
            title: "Extract into variable".to_string(),
            kind: Some("refactor.extract".to_string()),
            is_preferred: false,
            disabled_reason: Some("not applicable here".to_string()),
        }];

        app.trigger_direct_refactor(DirectRefactorKind::ExtractVariable);

        assert!(!app.via_refactor_preview);
        assert_eq!(
            app.error,
            Some("Extract Variable: not available here".to_string())
        );
    }

    #[test]
    fn trigger_direct_refactor_inline_matches_any_title_under_refactor_inline() {
        let mut app = app_without_gui();
        app.lsp.code_actions = vec![refactor_action(0, "Inline variable `x`", "refactor.inline")];

        app.trigger_direct_refactor(DirectRefactorKind::Inline);

        assert!(app.via_refactor_preview);
    }

    #[test]
    fn trigger_direct_refactor_with_no_match_sets_an_error_and_does_not_set_via_preview() {
        let mut app = app_without_gui();
        app.lsp.code_actions = vec![refactor_action(0, "Import `Foo`", "quickfix")];

        app.trigger_direct_refactor(DirectRefactorKind::ExtractMethod);

        assert!(!app.via_refactor_preview);
        assert_eq!(
            app.error,
            Some("Extract Method: not available here".to_string())
        );
    }

    #[test]
    fn apply_code_action_via_preview_sets_the_flag_and_does_not_panic_without_a_client() {
        let mut app = app_without_gui();
        app.apply_code_action_via_preview(0);
        assert!(app.via_refactor_preview);
    }

    #[test]
    fn trigger_generate_menu_with_no_generate_kind_action_sets_an_error() {
        let mut app = app_without_gui();
        app.lsp.code_actions = vec![refactor_action(0, "Import `Foo`", "quickfix")];

        app.trigger_generate_menu();

        assert!(!app.show_generate_menu_popup);
        assert_eq!(
            app.error,
            Some("Generate: nothing to generate here".to_string())
        );
    }

    #[test]
    fn trigger_generate_menu_opens_the_popup_without_sending_a_request() {
        let mut app = app_without_gui();
        app.lsp.code_actions = vec![refactor_action(0, "Generate constructor", "")];

        app.trigger_generate_menu();

        assert!(app.show_generate_menu_popup);
        assert_eq!(app.lsp.code_actions.len(), 1);
    }

    #[test]
    fn select_generate_action_closes_the_popup_and_applies_immediately() {
        let mut app = app_without_gui();
        app.show_generate_menu_popup = true;

        app.select_generate_action(0);

        assert!(!app.show_generate_menu_popup);
        assert!(!app.via_refactor_preview);
    }

    #[test]
    fn trigger_direct_generate_implement_methods_matches_quickfix_title() {
        let mut app = app_without_gui();
        app.lsp.code_actions = vec![refactor_action(0, "Implement missing members", "quickfix")];

        app.trigger_direct_generate(DirectGenerateKind::ImplementMethods);

        assert!(app.error.is_none());
    }

    #[test]
    fn trigger_direct_generate_override_methods_matches_quickfix_title() {
        let mut app = app_without_gui();
        app.lsp.code_actions = vec![refactor_action(0, "Implement default members", "quickfix")];

        app.trigger_direct_generate(DirectGenerateKind::OverrideMethods);

        assert!(app.error.is_none());
    }

    #[test]
    fn trigger_direct_generate_create_test_matches_on_title_alone() {
        let mut app = app_without_gui();
        app.lsp.code_actions = vec![refactor_action(0, "Generate test", "")];

        app.trigger_direct_generate(DirectGenerateKind::CreateTest);

        assert!(app.error.is_none());
    }

    #[test]
    fn trigger_direct_generate_skips_a_disabled_action() {
        let mut app = app_without_gui();
        app.lsp.code_actions = vec![CodeAction {
            index: 0,
            title: "Implement missing members".to_string(),
            kind: Some("quickfix".to_string()),
            is_preferred: false,
            disabled_reason: Some("not applicable here".to_string()),
        }];

        app.trigger_direct_generate(DirectGenerateKind::ImplementMethods);

        assert_eq!(
            app.error,
            Some("Implement Methods: not available here".to_string())
        );
    }

    #[test]
    fn trigger_direct_generate_with_no_match_sets_an_error() {
        let mut app = app_without_gui();
        app.lsp.code_actions = vec![refactor_action(0, "Import `Foo`", "quickfix")];

        app.trigger_direct_generate(DirectGenerateKind::OverrideMethods);

        assert_eq!(
            app.error,
            Some("Override Methods: not available here".to_string())
        );
    }

    #[test]
    fn trigger_optimize_imports_with_no_active_tab_does_not_panic() {
        let mut app = app_without_gui();
        app.trigger_optimize_imports();
    }

    #[test]
    fn trigger_optimize_imports_with_a_valid_target_does_not_panic_without_a_client() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.active_cursor_offset = Some(0);

        app.trigger_optimize_imports();
    }

    /// Inserts `new_text` at the start of `path` -- the `ide_core::
    /// WorkspaceEdit` counterpart to `workspace_edit_for` below, used by
    /// the Replace in Path preview tests since those edits are already
    /// `ide_core::FileEdit`s (no LSP `TextEdit` involved).
    fn core_workspace_edit_for(path: &Path, new_text: &str) -> ide_core::WorkspaceEdit {
        ide_core::WorkspaceEdit {
            edits: vec![ide_core::FileEdit {
                path: path.to_path_buf(),
                transaction: ide_core::text::Transaction::new(vec![ide_core::text::Change::new(
                    0..0,
                    new_text.to_string(),
                )])
                .unwrap(),
            }],
        }
    }

    fn workspace_edit_for(path: &Path, new_text: &str) -> ide_lsp::WorkspaceEdit {
        ide_lsp::WorkspaceEdit {
            edits: vec![ide_lsp::FileEdit {
                path: path.to_path_buf(),
                text_edits: vec![ide_lsp::TextEdit {
                    range: ide_lsp::Range {
                        start: Position {
                            line: 0,
                            character: 0,
                        },
                        end: Position {
                            line: 0,
                            character: 0,
                        },
                    },
                    new_text: new_text.to_string(),
                }],
            }],
        }
    }

    #[test]
    fn show_refactor_preview_diffs_an_open_tabs_buffer_without_writing_anything() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        let edit = workspace_edit_for(&file, "// hi\n");

        app.show_refactor_preview("Extract into variable".to_string(), edit);

        let preview = app.pending_refactor_preview.as_ref().unwrap();
        assert_eq!(preview.what, "Extract into variable");
        assert_eq!(preview.diffs.len(), 1);
        let diff = preview.diffs[0].as_ref().expect("diff computed");
        assert!(!diff.hunks.is_empty());
        // Read-only: the buffer and the on-disk file are both untouched.
        assert_eq!(
            app.tabs[app.active_tab.unwrap()].buffer.text(),
            "fn main() {}"
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "fn main() {}");
    }

    #[test]
    fn show_refactor_preview_diffs_a_file_with_no_open_tab_via_a_fresh_disk_read() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        let edit = workspace_edit_for(&file, "// hi\n");

        app.show_refactor_preview("Extract into function".to_string(), edit);

        let preview = app.pending_refactor_preview.as_ref().unwrap();
        let diff = preview.diffs[0].as_ref().expect("diff computed");
        assert!(!diff.hunks.is_empty());
    }

    #[test]
    fn show_refactor_preview_with_an_unreadable_file_yields_a_none_diff_entry_not_dropped() {
        let missing = PathBuf::from("/definitely/does/not/exist/f.rs");
        let mut app = app_without_gui();
        let edit = workspace_edit_for(&missing, "// hi\n");

        app.show_refactor_preview("Extract into variable".to_string(), edit);

        let preview = app.pending_refactor_preview.as_ref().unwrap();
        assert_eq!(preview.diffs.len(), 1);
        assert!(preview.diffs[0].is_none());
    }

    #[test]
    fn confirm_refactor_preview_applies_the_edit_and_clears_the_preview() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        let edit = workspace_edit_for(&file, "// hi\n");
        app.show_refactor_preview("Extract into variable".to_string(), edit);

        app.confirm_refactor_preview();

        assert!(app.pending_refactor_preview.is_none());
        assert_eq!(
            app.error,
            Some("Extract into variable: applied to 1 file".to_string())
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "// hi\nfn main() {}"
        );
    }

    #[test]
    fn confirm_refactor_preview_with_nothing_pending_is_a_noop() {
        let mut app = app_without_gui();
        app.confirm_refactor_preview();
        assert!(app.error.is_none());
    }

    #[test]
    fn cancel_refactor_preview_clears_without_writing_anything() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        let edit = workspace_edit_for(&file, "// hi\n");
        app.show_refactor_preview("Extract into variable".to_string(), edit);

        app.cancel_refactor_preview();

        assert!(app.pending_refactor_preview.is_none());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "fn main() {}");
    }

    #[test]
    fn show_replace_in_path_preview_diffs_an_open_tabs_buffer_without_writing_anything() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        let edit = core_workspace_edit_for(&file, "// hi\n");

        app.show_replace_in_path_preview(edit);

        let preview = app.pending_replace_in_path_preview.as_ref().unwrap();
        assert_eq!(preview.diffs.len(), 1);
        let diff = preview.diffs[0].as_ref().expect("diff computed");
        assert!(!diff.hunks.is_empty());
        // Read-only: the buffer and the on-disk file are both untouched.
        assert_eq!(
            app.tabs[app.active_tab.unwrap()].buffer.text(),
            "fn main() {}"
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "fn main() {}");
    }

    #[test]
    fn show_replace_in_path_preview_diffs_a_file_with_no_open_tab_via_a_fresh_disk_read() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        let edit = core_workspace_edit_for(&file, "// hi\n");

        app.show_replace_in_path_preview(edit);

        let preview = app.pending_replace_in_path_preview.as_ref().unwrap();
        let diff = preview.diffs[0].as_ref().expect("diff computed");
        assert!(!diff.hunks.is_empty());
    }

    #[test]
    fn show_replace_in_path_preview_with_an_unreadable_file_yields_a_none_diff_entry_not_dropped() {
        let missing = PathBuf::from("/definitely/does/not/exist/f.rs");
        let mut app = app_without_gui();
        let edit = core_workspace_edit_for(&missing, "// hi\n");

        app.show_replace_in_path_preview(edit);

        let preview = app.pending_replace_in_path_preview.as_ref().unwrap();
        assert_eq!(preview.diffs.len(), 1);
        assert!(preview.diffs[0].is_none());
    }

    #[test]
    fn confirm_replace_in_path_preview_applies_the_edit_and_clears_the_preview() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        let edit = core_workspace_edit_for(&file, "// hi\n");
        app.show_replace_in_path_preview(edit);

        app.confirm_replace_in_path_preview();

        assert!(app.pending_replace_in_path_preview.is_none());
        assert_eq!(
            app.error,
            Some("Replace in Path: applied to 1 file".to_string())
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "// hi\nfn main() {}"
        );
    }

    #[test]
    fn confirm_replace_in_path_preview_with_nothing_pending_is_a_noop() {
        let mut app = app_without_gui();
        app.confirm_replace_in_path_preview();
        assert!(app.error.is_none());
    }

    #[test]
    fn cancel_replace_in_path_preview_clears_without_writing_anything() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        let edit = core_workspace_edit_for(&file, "// hi\n");
        app.show_replace_in_path_preview(edit);

        app.cancel_replace_in_path_preview();

        assert!(app.pending_replace_in_path_preview.is_none());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "fn main() {}");
    }

    #[test]
    fn handle_workspace_edit_ready_via_preview_escalates_to_the_refactor_preview() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        app.via_refactor_preview = true;
        app.lsp.workspace_edit_ready = true;
        app.lsp.workspace_edit_label = Some("Extract into variable".to_string());
        app.lsp.workspace_edit = Some(workspace_edit_for(&file, "// hi\n"));

        app.handle_workspace_edit_ready();

        assert!(!app.via_refactor_preview);
        assert!(app.pending_refactor_preview.is_some());
        // Critical regression check: nothing was applied yet -- via_preview
        // must divert to the preview, not fall through to the ordinary
        // immediate-apply body.
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "fn main() {}");
        assert!(app.error.is_none());
    }

    #[test]
    fn handle_workspace_edit_ready_without_via_preview_applies_immediately_as_before() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        app.lsp.workspace_edit_ready = true;
        app.lsp.workspace_edit_label = Some("Import `Foo`".to_string());
        app.lsp.workspace_edit = Some(workspace_edit_for(&file, "// hi\n"));

        app.handle_workspace_edit_ready();

        assert!(app.pending_refactor_preview.is_none());
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "// hi\nfn main() {}"
        );
    }

    #[test]
    fn handle_workspace_edit_ready_resets_via_preview_even_with_no_edit_to_apply() {
        let mut app = app_without_gui();
        app.via_refactor_preview = true;
        app.lsp.workspace_edit_ready = true;
        app.lsp.workspace_edit_label = Some("Extract into variable".to_string());
        app.lsp.workspace_edit = None;

        app.handle_workspace_edit_ready();

        assert!(!app.via_refactor_preview);
        assert!(app.pending_refactor_preview.is_none());
        assert_eq!(
            app.error,
            Some("Extract into variable: nothing to apply".to_string())
        );
    }

    #[test]
    fn is_command_enabled_gates_refactor_this_and_direct_refactors_on_a_tab_with_a_path_and_a_running_server(
    ) {
        let mut app = app_without_gui();
        assert!(!app.is_command_enabled(CommandAction::RefactorThis));
        assert!(!app.is_command_enabled(CommandAction::ExtractVariable));

        app.new_untitled_tab();
        assert!(!app.is_command_enabled(CommandAction::RefactorThis));

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        app.open_file(&file);
        assert!(!app.is_command_enabled(CommandAction::Inline));
    }

    #[test]
    fn run_command_dispatches_refactor_this_and_extract_and_inline_commands() {
        let mut app = app_without_gui();
        app.lsp.code_actions = vec![refactor_action(
            0,
            "Extract into variable",
            "refactor.extract",
        )];
        let ctx = egui::Context::default();

        app.run_command(CommandAction::RefactorThis, &ctx);
        assert!(app.show_refactor_menu_popup);

        app.show_refactor_menu_popup = false;
        app.via_refactor_preview = false;
        app.run_command(CommandAction::ExtractVariable, &ctx);
        assert!(app.via_refactor_preview);
    }

    // ---- D1: rename refactoring ----

    #[test]
    fn trigger_rename_with_no_active_tab_is_a_noop() {
        let mut app = app_without_gui();
        app.trigger_rename();
        assert!(app.rename_popup.is_none());
        assert_eq!(app.error, None);
    }

    #[test]
    fn trigger_rename_with_untitled_tab_is_a_noop() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.trigger_rename();
        assert!(app.rename_popup.is_none());
        assert_eq!(app.error, None);
    }

    #[test]
    fn trigger_rename_with_no_running_language_server_sets_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.active_cursor_offset = Some(3);

        app.trigger_rename();

        assert!(app.rename_popup.is_none());
        assert_eq!(
            app.error,
            Some("Rename: no language server is running".to_string())
        );
    }

    #[test]
    fn trigger_rename_with_caret_off_a_symbol_sets_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "a + b").unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.lsp.sync_active_languages(
            dir.path(),
            &[ide_core::LanguageConfig {
                command: "cat".to_string(),
                ..ide_core::LanguageConfig::rust()
            }],
        );
        // Offset 2 is the `+` itself -- not inside any identifier.
        app.active_cursor_offset = Some(2);

        app.trigger_rename();

        assert!(app.rename_popup.is_none());
        assert_eq!(
            app.error,
            Some("Rename: no symbol under the caret".to_string())
        );
    }

    #[test]
    fn trigger_rename_opens_a_prefilled_popup_and_requests_prepare_rename() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.lsp.sync_active_languages(
            dir.path(),
            &[ide_core::LanguageConfig {
                command: "cat".to_string(),
                ..ide_core::LanguageConfig::rust()
            }],
        );
        app.active_cursor_offset = Some(3);

        app.trigger_rename();

        let popup = app
            .rename_popup
            .as_ref()
            .expect("expected the popup to open");
        assert_eq!(popup.path, file);
        assert_eq!(popup.original_name, "main");
        assert_eq!(popup.input, "main");
        assert_eq!(
            popup.position,
            Position {
                line: 0,
                character: 3
            }
        );
        assert_eq!(
            app.lsp.prepare_rename_target,
            Some((
                file,
                Position {
                    line: 0,
                    character: 3
                }
            ))
        );
        assert!(app.pending_rename_focus);
    }

    #[test]
    fn confirm_rename_with_no_popup_is_a_noop() {
        let mut app = app_without_gui();
        app.confirm_rename();
        assert!(app.rename_popup.is_none());
    }

    #[test]
    fn confirm_rename_with_unchanged_or_empty_input_sends_nothing_and_closes_silently() {
        let mut app = app_without_gui();
        app.rename_popup = Some(RenamePopup {
            path: PathBuf::from("/x.rs"),
            position: Position {
                line: 0,
                character: 3,
            },
            original_name: "main".to_string(),
            input: "main".to_string(),
        });
        app.confirm_rename();
        assert!(app.rename_popup.is_none());
        assert_eq!(app.error, None);

        app.rename_popup = Some(RenamePopup {
            path: PathBuf::from("/x.rs"),
            position: Position {
                line: 0,
                character: 3,
            },
            original_name: "main".to_string(),
            input: "   ".to_string(),
        });
        app.confirm_rename();
        assert!(app.rename_popup.is_none());
        assert_eq!(app.error, None);
    }

    /// Proves trimming happens *before* the unchanged-name comparison, not
    /// after: an input that only differs from `original_name` by
    /// leading/trailing whitespace must still take the silent-no-op path,
    /// which only holds if `confirm_rename` compares the trimmed value.
    /// (`bridge.request_rename` has no observable trace of what string it
    /// was called with once `send`'s no-op-if-no-client branch is skipped
    /// -- there's no running client here on purpose, so any bug that skips
    /// trimming and sends "main" padded would be indistinguishable from
    /// this test's happy path by any state assertion. This boundary case
    /// is the one place that distinction is actually observable: sending
    /// would still hit `LspBridge::request_rename`, but with no client
    /// running that's also silently a no-op, and to date the ide-lsp layer
    /// already round-trip-tests `new_name` transiting the wire unmodified
    /// -- `handle_rename_response_converts_the_edit_and_echoes_new_name`.)
    #[test]
    fn confirm_rename_trims_before_comparing_to_the_original_name() {
        let mut app = app_without_gui();
        app.rename_popup = Some(RenamePopup {
            path: PathBuf::from("/x.rs"),
            position: Position {
                line: 0,
                character: 3,
            },
            original_name: "main".to_string(),
            input: "  main  ".to_string(),
        });
        app.confirm_rename();
        assert!(app.rename_popup.is_none());
        assert_eq!(app.error, None);
    }

    #[test]
    fn confirm_rename_with_a_changed_name_closes_the_popup_and_sends_a_rename_request() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_without_gui();
        app.lsp.sync_active_languages(
            dir.path(),
            &[ide_core::LanguageConfig {
                command: "cat".to_string(),
                ..ide_core::LanguageConfig::rust()
            }],
        );
        app.rename_popup = Some(RenamePopup {
            path: dir.path().join("f.rs"),
            position: Position {
                line: 0,
                character: 3,
            },
            original_name: "main".to_string(),
            input: "  count  ".to_string(),
        });

        app.confirm_rename();

        assert!(app.rename_popup.is_none());
    }

    #[test]
    fn handle_prepare_rename_ready_is_a_noop_when_not_ready() {
        let mut app = app_without_gui();
        app.lsp.prepare_rename_ready = false;
        app.lsp.prepare_renameable = Some(false);
        app.rename_popup = Some(RenamePopup {
            path: PathBuf::from("/x.rs"),
            position: Position {
                line: 0,
                character: 0,
            },
            original_name: "x".to_string(),
            input: "x".to_string(),
        });
        app.handle_prepare_rename_ready();
        assert!(app.rename_popup.is_some());
    }

    #[test]
    fn handle_prepare_rename_ready_with_true_leaves_the_popup_open() {
        let path = PathBuf::from("/x.rs");
        let position = Position {
            line: 0,
            character: 0,
        };
        let mut app = app_without_gui();
        app.rename_popup = Some(RenamePopup {
            path: path.clone(),
            position,
            original_name: "x".to_string(),
            input: "x".to_string(),
        });
        app.lsp.prepare_rename_target = Some((path, position));
        app.lsp.prepare_renameable = Some(true);
        app.lsp.prepare_rename_ready = true;

        app.handle_prepare_rename_ready();

        assert!(app.rename_popup.is_some());
        assert_eq!(app.error, None);
    }

    #[test]
    fn handle_prepare_rename_ready_with_false_and_a_matching_target_closes_the_popup() {
        let path = PathBuf::from("/x.rs");
        let position = Position {
            line: 0,
            character: 0,
        };
        let mut app = app_without_gui();
        app.rename_popup = Some(RenamePopup {
            path: path.clone(),
            position,
            original_name: "x".to_string(),
            input: "x".to_string(),
        });
        app.lsp.prepare_rename_target = Some((path, position));
        app.lsp.prepare_renameable = Some(false);
        app.lsp.prepare_rename_ready = true;

        app.handle_prepare_rename_ready();

        assert!(app.rename_popup.is_none());
        assert_eq!(
            app.error,
            Some("Rename: this element cannot be renamed".to_string())
        );
    }

    #[test]
    fn handle_prepare_rename_ready_with_false_but_a_stale_target_is_a_noop() {
        // The popup was reopened for a different position after the
        // in-flight `PrepareRename` was sent -- this response no longer
        // answers what's currently open.
        let mut app = app_without_gui();
        app.rename_popup = Some(RenamePopup {
            path: PathBuf::from("/x.rs"),
            position: Position {
                line: 5,
                character: 0,
            },
            original_name: "y".to_string(),
            input: "y".to_string(),
        });
        app.lsp.prepare_rename_target = Some((
            PathBuf::from("/x.rs"),
            Position {
                line: 0,
                character: 0,
            },
        ));
        app.lsp.prepare_renameable = Some(false);
        app.lsp.prepare_rename_ready = true;

        app.handle_prepare_rename_ready();

        assert!(app.rename_popup.is_some());
        assert_eq!(app.error, None);
    }

    #[test]
    fn handle_prepare_rename_ready_with_false_and_no_popup_open_is_a_noop() {
        let mut app = app_without_gui();
        app.rename_popup = None;
        app.lsp.prepare_rename_target = Some((
            PathBuf::from("/x.rs"),
            Position {
                line: 0,
                character: 0,
            },
        ));
        app.lsp.prepare_renameable = Some(false);
        app.lsp.prepare_rename_ready = true;

        app.handle_prepare_rename_ready();

        assert!(app.rename_popup.is_none());
        assert_eq!(app.error, None);
    }

    #[test]
    fn handle_rename_ready_is_a_noop_when_not_ready() {
        let mut app = app_without_gui();
        app.lsp.rename_ready = false;
        app.lsp.rename_edit = Some(ide_lsp::WorkspaceEdit { edits: vec![] });

        app.handle_rename_ready();

        assert_eq!(app.error, None);
        assert!(app.lsp.rename_edit.is_some());
    }

    #[test]
    fn handle_rename_ready_with_no_edit_reports_nothing_to_apply() {
        let mut app = app_without_gui();
        app.lsp.rename_ready = true;
        app.lsp.rename_edit = None;
        app.lsp.rename_new_name = Some("count".to_string());

        app.handle_rename_ready();

        assert_eq!(
            app.error,
            Some("Rename to `count`: nothing to apply".to_string())
        );
    }

    #[test]
    fn handle_rename_ready_with_a_single_file_edit_matching_the_active_tab_applies_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.lsp.rename_ready = true;
        app.lsp.rename_new_name = Some("count".to_string());
        app.lsp.rename_edit = Some(ide_lsp::WorkspaceEdit {
            edits: vec![ide_lsp::FileEdit {
                path: file.clone(),
                text_edits: vec![ide_lsp::TextEdit {
                    range: ide_lsp::Range {
                        start: Position {
                            line: 0,
                            character: 0,
                        },
                        end: Position {
                            line: 0,
                            character: 0,
                        },
                    },
                    new_text: "// hi\n".to_string(),
                }],
            }],
        });

        app.handle_rename_ready();

        assert_eq!(
            app.error,
            Some("Rename to `count`: applied to 1 file".to_string())
        );
        assert_eq!(app.tabs[0].buffer.text(), "// hi\nfn main() {}");
        assert!(app.pending_rename_preview.is_none());
    }

    #[test]
    fn handle_rename_ready_with_multiple_files_escalates_to_preview() {
        let mut app = app_without_gui();
        app.lsp.rename_ready = true;
        app.lsp.rename_new_name = Some("count".to_string());
        let text_edit = ide_lsp::TextEdit {
            range: ide_lsp::Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 0,
                },
            },
            new_text: "count".to_string(),
        };
        app.lsp.rename_edit = Some(ide_lsp::WorkspaceEdit {
            edits: vec![
                ide_lsp::FileEdit {
                    path: PathBuf::from("/a.rs"),
                    text_edits: vec![text_edit.clone()],
                },
                ide_lsp::FileEdit {
                    path: PathBuf::from("/b.rs"),
                    text_edits: vec![text_edit],
                },
            ],
        });

        app.handle_rename_ready();

        assert_eq!(app.error, None);
        let (edit, new_name) = app
            .pending_rename_preview
            .clone()
            .expect("expected a preview");
        assert_eq!(edit.edits.len(), 2);
        assert_eq!(new_name, "count");
    }

    #[test]
    fn handle_rename_ready_with_a_single_file_not_matching_the_active_tab_escalates_to_preview() {
        let mut app = app_without_gui();
        app.lsp.rename_ready = true;
        app.lsp.rename_new_name = Some("count".to_string());
        app.lsp.rename_edit = Some(ide_lsp::WorkspaceEdit {
            edits: vec![ide_lsp::FileEdit {
                path: PathBuf::from("/a.rs"),
                text_edits: vec![ide_lsp::TextEdit {
                    range: ide_lsp::Range {
                        start: Position {
                            line: 0,
                            character: 0,
                        },
                        end: Position {
                            line: 0,
                            character: 0,
                        },
                    },
                    new_text: "count".to_string(),
                }],
            }],
        });

        app.handle_rename_ready();

        assert_eq!(app.error, None);
        assert!(app.pending_rename_preview.is_some());
    }

    #[test]
    fn is_command_enabled_gates_rename_on_a_tab_with_a_path_and_a_running_server() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        let mut app = app_without_gui();
        assert!(!app.is_command_enabled(CommandAction::Rename));

        app.tabs.push(Tab::untitled("Untitled".to_string()));
        app.active_tab = Some(0);
        assert!(!app.is_command_enabled(CommandAction::Rename));

        app.open_file(&file);
        // A path exists now, but no language server is running yet.
        assert!(!app.is_command_enabled(CommandAction::Rename));

        app.lsp.sync_active_languages(
            dir.path(),
            &[ide_core::LanguageConfig {
                command: "cat".to_string(),
                ..ide_core::LanguageConfig::rust()
            }],
        );
        assert!(app.is_command_enabled(CommandAction::Rename));
    }

    #[test]
    fn run_command_dispatches_rename() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.lsp.sync_active_languages(
            dir.path(),
            &[ide_core::LanguageConfig {
                command: "cat".to_string(),
                ..ide_core::LanguageConfig::rust()
            }],
        );
        app.active_cursor_offset = Some(3);
        let ctx = egui::Context::default();

        app.run_command(CommandAction::Rename, &ctx);

        assert!(app.rename_popup.is_some());
    }

    // ---- trigger_reformat_code / save_tab_at / handle_format_ready ----

    #[test]
    fn trigger_reformat_code_with_no_active_tab_is_a_noop() {
        let mut app = app_without_gui();
        app.trigger_reformat_code();
        assert!(!app.lsp.format_ready);
    }

    #[test]
    fn trigger_reformat_code_with_an_untitled_tab_is_a_noop() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.trigger_reformat_code();
        assert!(!app.lsp.format_ready);
    }

    #[test]
    fn trigger_reformat_code_with_no_client_running_self_resolves_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.trigger_reformat_code();

        assert!(app.lsp.format_ready);
        assert_eq!(app.lsp.format_edit, None);
        assert_eq!(app.lsp.format_path, Some(file));
    }

    #[test]
    fn save_tab_at_writes_the_given_index_not_the_active_tab() {
        let dir = tempfile::tempdir().unwrap();
        let file_a = dir.path().join("a.txt");
        let file_b = dir.path().join("b.txt");
        std::fs::write(&file_a, "a").unwrap();
        std::fs::write(&file_b, "b").unwrap();

        let mut app = app_without_gui();
        app.open_file(&file_a);
        app.open_file(&file_b);
        assert_eq!(app.active_tab, Some(1));

        app.tabs[0].buffer.insert(1, "!");
        let result = app.save_tab_at(0);

        assert!(matches!(result, Some(Ok(()))));
        assert_eq!(std::fs::read_to_string(&file_a).unwrap(), "a!");
        // The active tab (index 1) was never touched by this call.
        assert_eq!(std::fs::read_to_string(&file_b).unwrap(), "b");
    }

    #[test]
    fn handle_format_ready_is_a_noop_when_not_ready() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.handle_format_ready();
        assert!(app.error.is_none());
    }

    #[test]
    fn handle_format_ready_applies_edit_and_saves_when_format_on_save_target_matches() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.format_on_save_target = Some(file.clone());
        app.lsp.format_ready = true;
        app.lsp.format_path = Some(file.clone());
        app.lsp.format_edit = Some(ide_lsp::WorkspaceEdit {
            edits: vec![ide_lsp::FileEdit {
                path: file.clone(),
                text_edits: vec![ide_lsp::TextEdit {
                    range: ide_lsp::Range {
                        start: Position {
                            line: 0,
                            character: 0,
                        },
                        end: Position {
                            line: 0,
                            character: 0,
                        },
                    },
                    new_text: "// fmt\n".to_string(),
                }],
            }],
        });

        app.handle_format_ready();

        assert_eq!(app.tabs[0].buffer.text(), "// fmt\nfn main() {}");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "// fmt\nfn main() {}"
        );
        assert_eq!(app.format_on_save_target, None);
    }

    #[test]
    fn handle_format_ready_applies_edit_without_saving_when_no_target_is_set() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.lsp.format_ready = true;
        app.lsp.format_path = Some(file.clone());
        app.lsp.format_edit = Some(ide_lsp::WorkspaceEdit {
            edits: vec![ide_lsp::FileEdit {
                path: file.clone(),
                text_edits: vec![ide_lsp::TextEdit {
                    range: ide_lsp::Range {
                        start: Position {
                            line: 0,
                            character: 0,
                        },
                        end: Position {
                            line: 0,
                            character: 0,
                        },
                    },
                    new_text: "// fmt\n".to_string(),
                }],
            }],
        });

        app.handle_format_ready();

        assert_eq!(app.tabs[0].buffer.text(), "// fmt\nfn main() {}");
        // Not saved -- disk untouched, since `format_on_save_target` was
        // never set (a manual Reformat Code, not Format on Save).
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "fn main() {}");
    }

    #[test]
    fn handle_format_ready_with_no_edit_clears_target_and_applies_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.format_on_save_target = Some(file.clone());
        app.lsp.format_ready = true;
        app.lsp.format_path = Some(file.clone());
        app.lsp.format_edit = None;

        app.handle_format_ready();

        assert_eq!(app.tabs[0].buffer.text(), "fn main() {}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "fn main() {}");
        assert_eq!(app.format_on_save_target, None);
    }

    #[test]
    fn handle_format_ready_with_no_matching_open_tab_still_clears_target() {
        let mut app = app_without_gui();
        let path = PathBuf::from("/nonexistent/closed.rs");
        app.format_on_save_target = Some(path.clone());
        app.lsp.format_ready = true;
        app.lsp.format_path = Some(path.clone());
        app.lsp.format_edit = Some(ide_lsp::WorkspaceEdit {
            edits: vec![ide_lsp::FileEdit {
                path: path.clone(),
                text_edits: vec![ide_lsp::TextEdit {
                    range: ide_lsp::Range {
                        start: Position {
                            line: 0,
                            character: 0,
                        },
                        end: Position {
                            line: 0,
                            character: 0,
                        },
                    },
                    new_text: "x".to_string(),
                }],
            }],
        });

        app.handle_format_ready();

        assert_eq!(app.format_on_save_target, None);
    }

    #[test]
    fn maybe_trigger_format_on_save_does_not_fire_when_format_on_save_is_off() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.format_on_save = false;
        app.maybe_trigger_format_on_save();

        assert_eq!(app.format_on_save_target, None);
        assert!(!app.lsp.format_ready);
    }

    #[test]
    fn maybe_trigger_format_on_save_fires_and_sets_target_when_on() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.format_on_save = true;
        app.maybe_trigger_format_on_save();

        assert_eq!(app.format_on_save_target, Some(file.clone()));
        // No LSP client running in this test app, so `request_format`
        // self-resolves immediately (`docs/features/formatting.md` §2.3).
        assert!(app.lsp.format_ready);
        assert_eq!(app.lsp.format_path, Some(file));
    }

    #[test]
    fn try_save_active_fires_format_on_save_only_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.tabs[0].buffer.insert(0, "// x\n");
        app.format_on_save = false;
        app.try_save_active();
        assert_eq!(app.format_on_save_target, None);

        app.tabs[0].buffer.insert(0, "// y\n");
        app.format_on_save = true;
        app.try_save_active();
        assert_eq!(app.format_on_save_target, Some(file));
    }

    #[test]
    fn format_on_save_with_no_client_survives_poll_before_handle_format_ready() {
        // Reproduces the real per-frame order from `render.rs`'s `update()`:
        // `handle_shortcuts` (which can synchronously self-resolve
        // `request_format` via `SaveAll` -> `try_save_active` ->
        // `maybe_trigger_format_on_save`) runs before `self.lsp.poll()`,
        // which runs before `handle_format_ready()`. A regression test for
        // the bug where `poll()` used to unconditionally reset
        // `format_ready` at its top, clobbering a same-frame synchronous
        // set from before `poll()` ran and permanently orphaning
        // `format_on_save_target` since no LSP client was running to ever
        // send a real `FormatReady` event.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.tabs[0].buffer.insert(0, "// x\n");
        app.format_on_save = true;

        app.try_save_active();
        assert_eq!(app.format_on_save_target, Some(file.clone()));
        assert!(app.lsp.format_ready);

        app.lsp.poll();
        app.handle_format_ready();

        assert_eq!(app.format_on_save_target, None);
        assert!(!app.lsp.format_ready);
    }

    #[test]
    fn workspace_text_edits_to_transaction_converts_positions_to_byte_offsets() {
        let text = "fn main() {}";
        let edits = vec![ide_lsp::TextEdit {
            range: ide_lsp::Range {
                start: Position {
                    line: 0,
                    character: 3,
                },
                end: Position {
                    line: 0,
                    character: 7,
                },
            },
            new_text: "run".to_string(),
        }];

        let transaction = workspace_text_edits_to_transaction(text, &edits).unwrap();
        assert_eq!(transaction.changes().len(), 1);
        assert_eq!(transaction.changes()[0].range, 3..7);
        assert_eq!(transaction.changes()[0].insert, "run");
    }

    #[test]
    fn workspace_text_edits_to_transaction_rejects_a_position_past_the_end() {
        let text = "fn main() {}";
        let edits = vec![ide_lsp::TextEdit {
            range: ide_lsp::Range {
                start: Position {
                    line: 5,
                    character: 0,
                },
                end: Position {
                    line: 5,
                    character: 1,
                },
            },
            new_text: "x".to_string(),
        }];

        assert!(workspace_text_edits_to_transaction(text, &edits).is_none());
    }

    #[test]
    fn workspace_text_edits_to_transaction_rejects_overlapping_edits() {
        let text = "fn main() {}";
        let edits = vec![
            ide_lsp::TextEdit {
                range: ide_lsp::Range {
                    start: Position {
                        line: 0,
                        character: 0,
                    },
                    end: Position {
                        line: 0,
                        character: 5,
                    },
                },
                new_text: "a".to_string(),
            },
            ide_lsp::TextEdit {
                range: ide_lsp::Range {
                    start: Position {
                        line: 0,
                        character: 2,
                    },
                    end: Position {
                        line: 0,
                        character: 6,
                    },
                },
                new_text: "b".to_string(),
            },
        ];

        assert!(workspace_text_edits_to_transaction(text, &edits).is_none());
    }

    #[test]
    fn workspace_text_edits_to_transaction_accepts_an_end_of_file_insert_with_no_trailing_newline()
    {
        // `{line: doc_line_count, character: 0}` -- one line past the
        // file's last real line -- is a common LSP encoding for "insert at
        // end of file". Regression guard for a fix-round bug where this
        // was rejected outright (`docs/security-findings/rust-ui-dev-code-
        // actions-2026-08-20.md` finding 1's fix, first attempt): a
        // strictly line_index-bounded lookup treated this as out of range,
        // when `text.len()..text.len()` is the correct, in-bounds answer.
        let text = "one\ntwo"; // 2 lines, no trailing newline.
        let edits = vec![ide_lsp::TextEdit {
            range: ide_lsp::Range {
                start: Position {
                    line: 2,
                    character: 0,
                },
                end: Position {
                    line: 2,
                    character: 0,
                },
            },
            new_text: "\nTHREE".to_string(),
        }];

        let transaction = workspace_text_edits_to_transaction(text, &edits).unwrap();
        assert_eq!(transaction.changes()[0].range, text.len()..text.len());
    }

    #[test]
    fn workspace_text_edits_to_transaction_resolves_positions_on_later_lines_correctly() {
        let text = "one\ntwo\nthree\nfour";
        let edits = vec![ide_lsp::TextEdit {
            range: ide_lsp::Range {
                start: Position {
                    line: 2,
                    character: 0,
                },
                end: Position {
                    line: 2,
                    character: 5,
                },
            },
            new_text: "THREE".to_string(),
        }];

        let transaction = workspace_text_edits_to_transaction(text, &edits).unwrap();
        // "three" starts at byte 8 (after "one\n" (4) + "two\n" (4)).
        assert_eq!(transaction.changes()[0].range, 8..13);
    }

    #[test]
    fn workspace_text_edits_to_transaction_stays_fast_for_many_edits_across_many_lines() {
        // Regression test for docs/security-findings/rust-ui-dev-code-
        // actions-2026-08-20.md finding 1: the original implementation
        // called `ide_lsp::position_to_byte_offset` once per edit, which
        // re-scans the text from the start on every call -- O(N^2) for N
        // edits spread across an N-line file. 5,000 edits across a
        // 5,000-line file took over a second before the fix (extrapolated
        // from the finding's measured 1.4s at 16,000); this must complete
        // in well under that now that a single `LineIndex` is built once.
        let n = 5_000u32;
        let mut text = String::with_capacity(n as usize * 2);
        for _ in 0..n {
            text.push('x');
            text.push('\n');
        }
        let edits: Vec<ide_lsp::TextEdit> = (0..n)
            .map(|line| ide_lsp::TextEdit {
                range: ide_lsp::Range {
                    start: Position { line, character: 0 },
                    end: Position { line, character: 0 },
                },
                new_text: "y".to_string(),
            })
            .collect();

        let start = std::time::Instant::now();
        let transaction = workspace_text_edits_to_transaction(&text, &edits);
        let elapsed = start.elapsed();

        assert!(transaction.is_some());
        assert_eq!(transaction.unwrap().changes().len(), n as usize);
        assert!(
            elapsed.as_millis() < 500,
            "conversion took {elapsed:?} for {n} edits across {n} lines -- \
             expected well under 500ms from an O(text length + edit count) \
             conversion, not the O(N^2) the finding measured"
        );
    }

    #[test]
    fn is_command_enabled_gates_goto_commands_on_active_tab_and_navigate_on_history() {
        let mut app = app_without_gui();
        assert!(!app.is_command_enabled(CommandAction::GoToDeclaration));
        assert!(!app.is_command_enabled(CommandAction::GoToImplementation));
        assert!(!app.is_command_enabled(CommandAction::GoToTypeDeclaration));
        assert!(!app.is_command_enabled(CommandAction::NavigateBack));
        assert!(!app.is_command_enabled(CommandAction::NavigateForward));

        app.tabs.push(Tab::untitled("Untitled".to_string()));
        app.active_tab = Some(0);
        assert!(app.is_command_enabled(CommandAction::GoToDeclaration));
        assert!(app.is_command_enabled(CommandAction::GoToImplementation));
        assert!(app.is_command_enabled(CommandAction::GoToTypeDeclaration));
    }

    #[test]
    fn is_command_enabled_gates_reformat_code_on_a_tab_with_a_path() {
        let mut app = app_without_gui();
        assert!(!app.is_command_enabled(CommandAction::ReformatCode));
        assert!(app.is_command_enabled(CommandAction::ToggleFormatOnSave));

        app.tabs.push(Tab::untitled("Untitled".to_string()));
        app.active_tab = Some(0);
        // An untitled tab has no path -- still disabled, unlike GoTo*.
        assert!(!app.is_command_enabled(CommandAction::ReformatCode));

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        app.open_file(&file);
        assert!(app.is_command_enabled(CommandAction::ReformatCode));
    }

    #[test]
    fn run_command_dispatches_reformat_code_and_toggle_format_on_save() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let file = file.canonicalize().unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        let ctx = egui::Context::default();

        assert!(!app.format_on_save);
        app.run_command(CommandAction::ToggleFormatOnSave, &ctx);
        assert!(app.format_on_save);
        app.run_command(CommandAction::ToggleFormatOnSave, &ctx);
        assert!(!app.format_on_save);

        app.run_command(CommandAction::ReformatCode, &ctx);
        assert!(app.lsp.format_ready);
    }

    #[test]
    fn run_cargo_with_no_project_is_a_noop() {
        let mut app = app_without_gui();
        app.run_cargo(CargoCommand::Build);
        assert!(app.cargo.running.is_none());
    }

    // ---- A5: in-buffer find/replace ----

    #[test]
    fn open_find_with_no_selection_reuses_the_existing_query() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.tabs[0].buffer.insert(0, "needle in a haystack");
        app.open_find();
        assert!(app.tabs[0].find.is_open());
        assert!(!app.tabs[0].find.replace_open());
        assert_eq!(app.tabs[0].find.query(), "");
        assert!(app.pending_find_focus);
    }

    #[test]
    fn open_find_seeds_from_a_single_line_selection() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.tabs[0].buffer.insert(0, "needle in a haystack");
        app.tabs[0]
            .buffer
            .text_buffer_mut()
            .set_selections(Selections::single(Selection::new(0, 6)));
        app.open_find();
        assert_eq!(app.tabs[0].find.query(), "needle");
        assert_eq!(app.tabs[0].find.matches().len(), 1);
    }

    #[test]
    fn open_find_does_not_seed_from_a_multiline_selection() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.tabs[0].buffer.insert(0, "one\ntwo");
        app.tabs[0]
            .buffer
            .text_buffer_mut()
            .set_selections(Selections::single(Selection::new(0, 7)));
        app.open_find();
        assert_eq!(app.tabs[0].find.query(), "");
    }

    #[test]
    fn open_replace_reveals_the_row_and_open_find_never_hides_it_again() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.open_replace();
        assert!(app.tabs[0].find.replace_open());
        app.open_find();
        assert!(app.tabs[0].find.replace_open());
    }

    #[test]
    fn open_find_with_no_active_tab_is_a_noop() {
        let mut app = app_without_gui();
        app.open_find();
        assert!(app.active_tab.is_none());
    }

    #[test]
    fn close_find_clears_matches_but_keeps_the_query() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.tabs[0].buffer.insert(0, "x x x");
        app.open_find();
        let text = app.tabs[0].buffer.text().to_string();
        app.tabs[0].find.set_query("x".to_string(), &text);
        app.close_find();
        assert!(!app.tabs[0].find.is_open());
        assert_eq!(app.tabs[0].find.query(), "x");
        assert!(app.tabs[0].find.matches().is_empty());
    }

    #[test]
    fn find_next_moves_the_selection_to_the_next_match_and_scrolls() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.tabs[0].buffer.insert(0, "x ax bx cx");
        app.open_find();
        let text = app.tabs[0].buffer.text().to_string();
        app.tabs[0].find.set_query("x".to_string(), &text);
        // `set_query`'s search already lands `current` on the first match
        // (0..1), so the first `find_next` advances past it to the second.
        app.find_next();
        let selection = app.tabs[0].buffer.text_buffer().selections().primary();
        assert_eq!(selection.range(), 3..4);
        app.find_next();
        let selection = app.tabs[0].buffer.text_buffer().selections().primary();
        assert_eq!(selection.range(), 6..7);
    }

    #[test]
    fn find_next_with_the_bar_closed_is_a_noop() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.tabs[0].buffer.insert(0, "x x");
        let before = app.tabs[0].buffer.text_buffer().selections().primary();
        app.find_next();
        let after = app.tabs[0].buffer.text_buffer().selections().primary();
        assert_eq!(before, after);
    }

    #[test]
    fn find_previous_wraps_to_the_last_match() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.tabs[0].buffer.insert(0, "x x x");
        app.open_find();
        let text = app.tabs[0].buffer.text().to_string();
        app.tabs[0].find.set_query("x".to_string(), &text);
        app.find_previous();
        let selection = app.tabs[0].buffer.text_buffer().selections().primary();
        assert_eq!(selection.range(), 4..5);
    }

    #[test]
    fn replace_current_match_applies_one_undoable_edit_and_advances() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.tabs[0].buffer.insert(0, "foo foo");
        app.open_find();
        let text = app.tabs[0].buffer.text().to_string();
        app.tabs[0].find.set_query("foo".to_string(), &text);
        app.tabs[0].find.set_replacement("bar".to_string());

        app.replace_current_match();
        assert_eq!(app.tabs[0].buffer.text(), "bar foo");
        assert!(app.tabs[0].buffer.undo());
        assert_eq!(app.tabs[0].buffer.text(), "foo foo");
    }

    #[test]
    fn replace_current_match_with_no_current_match_is_a_noop() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.tabs[0].buffer.insert(0, "abc");
        app.open_find();
        app.replace_current_match();
        assert_eq!(app.tabs[0].buffer.text(), "abc");
    }

    #[test]
    fn replace_all_matches_applies_every_match_as_one_undo_step() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.tabs[0].buffer.insert(0, "foo foo foo");
        app.open_find();
        let text = app.tabs[0].buffer.text().to_string();
        app.tabs[0].find.set_query("foo".to_string(), &text);
        app.tabs[0].find.set_replacement("bar".to_string());

        app.replace_all_matches();
        assert_eq!(app.tabs[0].buffer.text(), "bar bar bar");
        assert!(app.tabs[0].buffer.undo());
        assert_eq!(app.tabs[0].buffer.text(), "foo foo foo");
        assert!(app.error.is_none());
    }

    #[test]
    fn replace_all_matches_signals_truncation_instead_of_silencing_it() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.tabs[0]
            .buffer
            .insert(0, &"x".repeat(ide_core::MAX_SEARCH_MATCHES + 1));
        app.open_find();
        let text = app.tabs[0].buffer.text().to_string();
        app.tabs[0].find.set_query("x".to_string(), &text);
        app.tabs[0].find.set_replacement("y".to_string());

        app.replace_all_matches();
        assert!(app.error.is_some());
        assert!(app.error.as_ref().unwrap().contains("Replace All again"));
        assert_eq!(
            app.tabs[0].buffer.text(),
            format!("{}x", "y".repeat(ide_core::MAX_SEARCH_MATCHES))
        );
    }

    #[test]
    fn replace_all_matches_with_no_matches_is_a_noop() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.tabs[0].buffer.insert(0, "abc");
        app.open_find();
        app.replace_all_matches();
        assert_eq!(app.tabs[0].buffer.text(), "abc");
    }

    #[test]
    fn replace_all_matches_scoped_to_the_selection_only_touches_it() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.tabs[0].buffer.insert(0, "foo foo foo");
        app.open_find();
        let text = app.tabs[0].buffer.text().to_string();
        app.tabs[0].find.set_query("foo".to_string(), &text);
        app.tabs[0].find.set_replacement("bar".to_string());
        app.tabs[0].find.set_scope(Some(4..7), &text);

        app.replace_all_matches();
        assert_eq!(app.tabs[0].buffer.text(), "foo bar foo");
    }

    #[test]
    fn run_command_dispatches_replace_all() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.tabs[0].buffer.insert(0, "foo foo foo");
        app.open_find();
        let text = app.tabs[0].buffer.text().to_string();
        app.tabs[0].find.set_query("foo".to_string(), &text);
        app.tabs[0].find.set_replacement("bar".to_string());
        let ctx = egui::Context::default();

        app.run_command(CommandAction::ReplaceAll, &ctx);

        assert_eq!(app.tabs[0].buffer.text(), "bar bar bar");
    }

    #[test]
    fn replace_all_is_enabled_only_with_an_active_tab() {
        let mut app = app_without_gui();
        assert!(!app.is_command_enabled(CommandAction::ReplaceAll));
        app.new_untitled_tab();
        assert!(app.is_command_enabled(CommandAction::ReplaceAll));
    }

    #[test]
    fn reload_from_disk_refreshes_an_open_find_bar_against_the_new_text() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "foo").unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.open_find();
        let text = app.tabs[0].buffer.text().to_string();
        app.tabs[0].find.set_query("bar".to_string(), &text);
        assert!(app.tabs[0].find.matches().is_empty());

        std::fs::write(&file, "bar bar").unwrap();
        app.reload_active_from_disk();

        assert_eq!(app.tabs[0].find.matches().len(), 2);
    }

    // ---- B3: command registry & palette ----

    #[test]
    fn open_command_palette_resets_query_selection_and_requests_focus() {
        let mut app = app_without_gui();
        app.command_palette_query = "stale".to_string();
        app.command_palette_selected = 3;

        app.open_command_palette();

        assert!(app.command_palette_open);
        assert!(app.command_palette_query.is_empty());
        assert_eq!(app.command_palette_selected, 0);
        assert!(app.pending_command_palette_focus);
    }

    #[test]
    fn close_command_palette_clears_open_and_query() {
        let mut app = app_without_gui();
        app.open_command_palette();
        app.command_palette_query = "find".to_string();

        app.close_command_palette();

        assert!(!app.command_palette_open);
        assert!(app.command_palette_query.is_empty());
    }

    #[test]
    fn filtered_commands_with_empty_query_returns_every_command_in_declaration_order() {
        let app = app_without_gui();
        let titles: Vec<&str> = app.filtered_commands().iter().map(|c| c.title).collect();
        assert_eq!(titles.len(), command::commands().len());
        assert_eq!(titles[0], "Save All");
    }

    #[test]
    fn filtered_commands_matches_title_case_insensitively() {
        let mut app = app_without_gui();
        app.command_palette_query = "SAVE".to_string();
        let titles: Vec<&str> = app.filtered_commands().iter().map(|c| c.title).collect();
        assert_eq!(titles, vec!["Save All", "Toggle Format on Save"]);
    }

    #[test]
    fn filtered_commands_also_matches_against_category() {
        let mut app = app_without_gui();
        app.command_palette_query = "navigate".to_string();
        let titles: Vec<&str> = app.filtered_commands().iter().map(|c| c.title).collect();
        assert!(titles.contains(&"Find Usages"));
        assert!(titles.contains(&"Find Action"));
        assert!(!titles.contains(&"Save All"));
    }

    #[test]
    fn command_palette_move_selection_wraps_both_directions() {
        let mut app = app_without_gui();
        let len = app.filtered_commands().len();
        app.command_palette_selected = 0;

        app.command_palette_move_selection(-1);
        assert_eq!(app.command_palette_selected, len - 1);

        app.command_palette_move_selection(1);
        assert_eq!(app.command_palette_selected, 0);
    }

    #[test]
    fn command_palette_move_selection_is_a_noop_on_an_empty_filtered_list() {
        let mut app = app_without_gui();
        app.command_palette_query = "zzz_no_such_command".to_string();
        app.command_palette_selected = 0;

        app.command_palette_move_selection(1);

        assert_eq!(app.command_palette_selected, 0);
    }

    #[test]
    fn command_palette_confirm_on_a_disabled_row_stays_open_and_does_nothing() {
        let mut app = app_without_gui();
        app.open_command_palette();
        app.command_palette_query = "save".to_string();
        app.command_palette_selected = 0;

        // No active tab -> SaveAll is disabled.
        app.command_palette_confirm(&egui::Context::default());

        assert!(app.command_palette_open);
    }

    #[test]
    fn command_palette_confirm_on_an_enabled_row_runs_it_and_closes() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.open_command_palette();
        app.command_palette_query = "undo".to_string();
        app.command_palette_selected = 0;

        app.command_palette_confirm(&egui::Context::default());

        assert!(!app.command_palette_open);
    }

    #[test]
    fn command_palette_confirm_on_find_action_resets_instead_of_closing() {
        let mut app = app_without_gui();
        app.open_command_palette();
        app.command_palette_query = "find action".to_string();
        app.command_palette_selected = 0;

        app.command_palette_confirm(&egui::Context::default());

        assert!(app.command_palette_open);
        assert!(app.command_palette_query.is_empty());
    }

    // ---- C2: Search Everywhere ----

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
    fn open_search_everywhere_resets_state_and_sets_tab_and_class_filter() {
        let mut app = app_without_gui();
        app.search_everywhere_query = "stale".to_string();
        app.search_everywhere_selected = 3;

        app.open_search_everywhere(SearchEverywhereTab::Symbols, true);

        assert!(app.search_everywhere_open);
        assert_eq!(app.search_everywhere_tab, SearchEverywhereTab::Symbols);
        assert!(app.search_everywhere_query.is_empty());
        assert_eq!(app.search_everywhere_selected, 0);
        assert!(app.pending_search_everywhere_focus);
        assert!(app.search_everywhere_class_filter);
    }

    #[test]
    fn close_search_everywhere_closes_and_resets_class_filter() {
        let mut app = app_without_gui();
        app.open_search_everywhere(SearchEverywhereTab::Symbols, true);

        app.close_search_everywhere();

        assert!(!app.search_everywhere_open);
        assert!(!app.search_everywhere_class_filter);
    }

    #[test]
    fn search_everywhere_owns_escape_true_only_while_open() {
        let mut app = app_without_gui();
        assert!(!app.search_everywhere_owns_escape());
        app.open_search_everywhere(SearchEverywhereTab::Files, false);
        assert!(app.search_everywhere_owns_escape());
    }

    #[test]
    fn search_everywhere_switch_tab_cycles_both_directions_and_resets_selection() {
        let mut app = app_without_gui();
        app.search_everywhere_tab = SearchEverywhereTab::Files;
        app.search_everywhere_selected = 2;

        app.search_everywhere_switch_tab(1);
        assert_eq!(app.search_everywhere_tab, SearchEverywhereTab::Symbols);
        assert_eq!(app.search_everywhere_selected, 0);

        app.search_everywhere_switch_tab(1);
        assert_eq!(app.search_everywhere_tab, SearchEverywhereTab::Actions);

        app.search_everywhere_switch_tab(-1);
        assert_eq!(app.search_everywhere_tab, SearchEverywhereTab::Symbols);

        app.search_everywhere_tab = SearchEverywhereTab::Files;
        app.search_everywhere_switch_tab(-1);
        assert_eq!(app.search_everywhere_tab, SearchEverywhereTab::Text);
    }

    #[test]
    fn sync_search_everywhere_is_a_noop_when_not_open() {
        let mut app = app_without_gui();
        app.search_everywhere_tab = SearchEverywhereTab::Files;
        app.search_everywhere_query = "x".to_string();
        app.sync_search_everywhere();
        assert!(app.last_files_query.is_none());
    }

    #[test]
    fn sync_search_everywhere_files_tab_runs_once_and_yields_results() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("needle.txt"), "x").unwrap();
        let project = Project::open(dir.path()).unwrap();

        let mut app = app_without_gui();
        app.load_project(project, &egui::Context::default());
        app.open_search_everywhere(SearchEverywhereTab::Files, false);
        app.search_everywhere_query = "needle".to_string();

        app.sync_search_everywhere();
        assert_eq!(
            app.last_files_query.as_deref(),
            Some("needle"),
            "run should have launched synchronously, marking the query as sent"
        );

        wait_until(|| {
            app.search_everywhere_files.poll();
            !app.search_everywhere_files.searching
        });

        let rows = app.search_everywhere_rows();
        assert_eq!(rows.len(), 1);
        assert!(matches!(&rows[0], SearchEverywhereRow::File(m) if m.relative == "needle.txt"));

        // An unchanged query does not re-trigger a search.
        app.search_everywhere_files.results = None;
        app.sync_search_everywhere();
        assert!(app.search_everywhere_files.results.is_none());
    }

    #[test]
    fn sync_search_everywhere_files_tab_is_a_noop_with_no_project() {
        let mut app = app_without_gui();
        app.open_search_everywhere(SearchEverywhereTab::Files, false);
        app.search_everywhere_query = "needle".to_string();
        app.sync_search_everywhere();
        assert!(app.last_files_query.is_none());
    }

    #[test]
    fn sync_search_everywhere_symbols_tab_empty_query_requests_document_symbols_once_per_path() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.open_search_everywhere(SearchEverywhereTab::Symbols, false);

        app.sync_search_everywhere();
        assert_eq!(
            app.document_symbols_requested_for,
            Some(IdeApp::canonicalize_best_effort(&file))
        );
    }

    #[test]
    fn sync_search_everywhere_symbols_tab_nonempty_query_tracks_the_last_sent_query() {
        let mut app = app_without_gui();
        app.open_search_everywhere(SearchEverywhereTab::Symbols, false);
        app.search_everywhere_query = "foo".to_string();

        app.sync_search_everywhere();
        assert_eq!(app.last_workspace_symbol_query.as_deref(), Some("foo"));
    }

    fn dummy_symbol(name: &str, kind: SymbolKind, path: &std::path::Path) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind,
            container_name: None,
            location: Location {
                path: path.to_path_buf(),
                range: ide_lsp::Range {
                    start: Position {
                        line: 0,
                        character: 0,
                    },
                    end: Position {
                        line: 0,
                        character: 0,
                    },
                },
            },
        }
    }

    #[test]
    fn search_everywhere_rows_symbols_uses_document_symbols_for_empty_query() {
        let mut app = app_without_gui();
        let path = PathBuf::from("/a.rs");
        app.lsp.document_symbols = vec![dummy_symbol("Foo", SymbolKind::Struct, &path)];
        app.lsp.workspace_symbols = vec![dummy_symbol("Bar", SymbolKind::Function, &path)];
        app.search_everywhere_tab = SearchEverywhereTab::Symbols;
        app.search_everywhere_query.clear();

        let rows = app.search_everywhere_rows();
        assert_eq!(rows.len(), 1);
        assert!(matches!(&rows[0], SearchEverywhereRow::Symbol(s) if s.name == "Foo"));
    }

    #[test]
    fn search_everywhere_rows_symbols_uses_workspace_symbols_for_a_nonempty_query() {
        let mut app = app_without_gui();
        let path = PathBuf::from("/a.rs");
        app.lsp.document_symbols = vec![dummy_symbol("Foo", SymbolKind::Struct, &path)];
        app.lsp.workspace_symbols = vec![dummy_symbol("Bar", SymbolKind::Function, &path)];
        app.search_everywhere_tab = SearchEverywhereTab::Symbols;
        app.search_everywhere_query = "b".to_string();

        let rows = app.search_everywhere_rows();
        assert_eq!(rows.len(), 1);
        assert!(matches!(&rows[0], SearchEverywhereRow::Symbol(s) if s.name == "Bar"));
    }

    #[test]
    fn search_everywhere_rows_symbols_class_filter_keeps_only_class_like_kinds() {
        let mut app = app_without_gui();
        let path = PathBuf::from("/a.rs");
        app.lsp.document_symbols = vec![
            dummy_symbol("Foo", SymbolKind::Struct, &path),
            dummy_symbol("bar", SymbolKind::Function, &path),
        ];
        app.search_everywhere_tab = SearchEverywhereTab::Symbols;
        app.search_everywhere_query.clear();
        app.search_everywhere_class_filter = true;

        let rows = app.search_everywhere_rows();
        assert_eq!(rows.len(), 1);
        assert!(matches!(&rows[0], SearchEverywhereRow::Symbol(s) if s.name == "Foo"));
    }

    #[test]
    fn search_everywhere_rows_actions_scores_and_sorts_by_fuzzy_score() {
        let mut app = app_without_gui();
        app.search_everywhere_tab = SearchEverywhereTab::Actions;
        app.search_everywhere_query = "save".to_string();

        let rows = app.search_everywhere_rows();
        assert!(!rows.is_empty());
        assert!(matches!(&rows[0], SearchEverywhereRow::Action(cmd) if cmd.title == "Save All"));
    }

    #[test]
    fn search_everywhere_rows_text_returns_the_text_panels_matches() {
        let mut app = app_without_gui();
        app.search_everywhere_tab = SearchEverywhereTab::Text;
        app.search_everywhere_text.results = Some(ide_core::SearchResults {
            matches: vec![ide_core::SearchMatch {
                path: PathBuf::from("/a.txt"),
                line: 0,
                column: 0,
                byte_offset: 0,
                line_text: "hit".to_string(),
            }],
            truncated: false,
        });

        let rows = app.search_everywhere_rows();
        assert_eq!(rows.len(), 1);
        assert!(matches!(&rows[0], SearchEverywhereRow::Text(m) if m.line_text == "hit"));
    }

    #[test]
    fn search_everywhere_move_selection_wraps_both_directions() {
        let mut app = app_without_gui();
        app.search_everywhere_tab = SearchEverywhereTab::Actions;
        app.search_everywhere_query.clear();
        let len = app.search_everywhere_rows().len();
        app.search_everywhere_selected = 0;

        app.search_everywhere_move_selection(-1);
        assert_eq!(app.search_everywhere_selected, len - 1);

        app.search_everywhere_move_selection(1);
        assert_eq!(app.search_everywhere_selected, 0);
    }

    #[test]
    fn search_everywhere_move_selection_is_a_noop_on_an_empty_row_list() {
        let mut app = app_without_gui();
        app.search_everywhere_tab = SearchEverywhereTab::Text;
        app.search_everywhere_selected = 0;

        app.search_everywhere_move_selection(1);

        assert_eq!(app.search_everywhere_selected, 0);
    }

    #[test]
    fn search_everywhere_confirm_file_row_opens_the_file_and_closes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        let mut app = app_without_gui();
        app.open_search_everywhere(SearchEverywhereTab::Files, false);
        app.search_everywhere_files.results = Some(ide_core::FuzzyFileResults {
            matches: vec![ide_core::FuzzyFileMatch {
                path: file.clone(),
                relative: "f.rs".to_string(),
                score: 0,
                indices: vec![],
            }],
            truncated: false,
        });

        app.search_everywhere_confirm(&egui::Context::default());

        assert_eq!(app.tabs.len(), 1);
        assert!(!app.search_everywhere_open);
    }

    #[test]
    fn search_everywhere_confirm_symbol_row_opens_the_definition_and_closes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        let mut app = app_without_gui();
        app.open_search_everywhere(SearchEverywhereTab::Symbols, false);
        app.lsp.document_symbols = vec![Symbol {
            name: "main".to_string(),
            kind: SymbolKind::Function,
            container_name: None,
            location: Location {
                path: file.clone(),
                range: ide_lsp::Range {
                    start: Position {
                        line: 0,
                        character: 3,
                    },
                    end: Position {
                        line: 0,
                        character: 7,
                    },
                },
            },
        }];

        app.search_everywhere_confirm(&egui::Context::default());

        assert_eq!(app.pending_cursor_offset, Some(3));
        assert!(!app.search_everywhere_open);
    }

    #[test]
    fn search_everywhere_confirm_text_row_opens_the_search_result_and_closes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "hello world").unwrap();

        let mut app = app_without_gui();
        app.open_search_everywhere(SearchEverywhereTab::Text, false);
        app.search_everywhere_text.results = Some(ide_core::SearchResults {
            matches: vec![ide_core::SearchMatch {
                path: file.clone(),
                line: 0,
                column: 6,
                byte_offset: 6,
                line_text: "hello world".to_string(),
            }],
            truncated: false,
        });

        app.search_everywhere_confirm(&egui::Context::default());

        assert_eq!(app.pending_cursor_offset, Some(6));
        assert!(!app.search_everywhere_open);
    }

    #[test]
    fn search_everywhere_confirm_disabled_action_row_stays_open() {
        let mut app = app_without_gui();
        app.open_search_everywhere(SearchEverywhereTab::Actions, false);
        app.search_everywhere_query = "save".to_string();
        app.search_everywhere_selected = 0;

        // No active tab -> Save All is disabled.
        app.search_everywhere_confirm(&egui::Context::default());

        assert!(app.search_everywhere_open);
    }

    #[test]
    fn search_everywhere_confirm_enabled_action_row_runs_it_and_closes() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.open_search_everywhere(SearchEverywhereTab::Actions, false);
        app.search_everywhere_query = "undo".to_string();
        app.search_everywhere_selected = 0;

        app.search_everywhere_confirm(&egui::Context::default());

        assert!(!app.search_everywhere_open);
    }

    #[test]
    fn trigger_go_to_line_opens_the_dialog_and_resets_input() {
        let mut app = app_without_gui();
        app.go_to_line_input = "stale".to_string();

        app.trigger_go_to_line();

        assert!(app.show_go_to_line);
        assert!(app.go_to_line_input.is_empty());
        assert!(app.pending_go_to_line_focus);
    }

    #[test]
    fn confirm_go_to_line_moves_the_cursor_to_the_parsed_line_and_column() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "aaa\nbbbbb\nccc").unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.show_go_to_line = true;
        app.go_to_line_input = "2:3".to_string();

        app.confirm_go_to_line();

        // Line 2 ("bbbbb") starts at byte 4; column 3 is 2 chars in.
        assert_eq!(app.pending_cursor_offset, Some(6));
        assert!(!app.show_go_to_line);
    }

    #[test]
    fn confirm_go_to_line_defaults_the_column_to_one() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "aaa\nbbbbb\nccc").unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.show_go_to_line = true;
        app.go_to_line_input = "2".to_string();

        app.confirm_go_to_line();

        assert_eq!(app.pending_cursor_offset, Some(4));
    }

    #[test]
    fn confirm_go_to_line_clamps_a_line_past_the_end_to_the_last_line() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "aaa\nbbbbb\nccc").unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.show_go_to_line = true;
        app.go_to_line_input = "99".to_string();

        app.confirm_go_to_line();

        assert_eq!(app.pending_cursor_offset, Some(10));
    }

    #[test]
    fn confirm_go_to_line_clamps_a_column_past_the_lines_end() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "aaa\nbbbbb\nccc").unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.show_go_to_line = true;
        app.go_to_line_input = "1:99".to_string();

        app.confirm_go_to_line();

        assert_eq!(app.pending_cursor_offset, Some(3));
    }

    #[test]
    fn confirm_go_to_line_is_a_noop_on_malformed_input() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "aaa\nbbbbb\nccc").unwrap();

        for input in ["", "abc", "0", ":5", "1:0", "1:abc"] {
            let mut app = app_without_gui();
            app.open_file(&file);
            app.show_go_to_line = true;
            app.go_to_line_input = input.to_string();

            app.confirm_go_to_line();

            assert_eq!(
                app.pending_cursor_offset, None,
                "input {input:?} should be a no-op"
            );
            assert!(
                app.show_go_to_line,
                "input {input:?} should leave the dialog open"
            );
        }
    }

    #[test]
    fn confirm_go_to_line_is_a_noop_with_no_active_tab() {
        let mut app = app_without_gui();
        app.show_go_to_line = true;
        app.go_to_line_input = "1".to_string();

        app.confirm_go_to_line();

        assert_eq!(app.pending_cursor_offset, None);
        assert!(app.show_go_to_line);
    }

    #[test]
    fn is_class_like_symbol_matches_only_class_like_kinds() {
        assert!(is_class_like_symbol(SymbolKind::Class));
        assert!(is_class_like_symbol(SymbolKind::Struct));
        assert!(is_class_like_symbol(SymbolKind::Interface));
        assert!(is_class_like_symbol(SymbolKind::Enum));
        assert!(!is_class_like_symbol(SymbolKind::Function));
        assert!(!is_class_like_symbol(SymbolKind::Variable));
    }

    #[test]
    fn is_command_enabled_gates_go_to_file_class_symbol_on_project_and_go_to_line_on_editor_tab() {
        let mut app = app_without_gui();
        assert!(!app.is_command_enabled(CommandAction::GoToFile));
        assert!(!app.is_command_enabled(CommandAction::GoToClass));
        assert!(!app.is_command_enabled(CommandAction::GoToSymbol));
        assert!(!app.is_command_enabled(CommandAction::GoToLine));

        app.new_untitled_tab();
        assert!(app.is_command_enabled(CommandAction::GoToLine));
        assert!(!app.is_command_enabled(CommandAction::GoToFile));

        let dir = tempfile::tempdir().unwrap();
        let project = Project::create(dir.path().join("proj")).unwrap();
        app.load_project(project, &egui::Context::default());
        assert!(app.is_command_enabled(CommandAction::GoToFile));
        assert!(app.is_command_enabled(CommandAction::GoToClass));
        assert!(app.is_command_enabled(CommandAction::GoToSymbol));
    }

    #[test]
    fn run_command_go_to_file_class_symbol_open_the_popup_on_the_right_tab() {
        let mut app = app_without_gui();

        app.run_command(CommandAction::GoToFile, &egui::Context::default());
        assert_eq!(app.search_everywhere_tab, SearchEverywhereTab::Files);
        assert!(!app.search_everywhere_class_filter);

        app.run_command(CommandAction::GoToClass, &egui::Context::default());
        assert_eq!(app.search_everywhere_tab, SearchEverywhereTab::Symbols);
        assert!(app.search_everywhere_class_filter);

        app.run_command(CommandAction::GoToSymbol, &egui::Context::default());
        assert_eq!(app.search_everywhere_tab, SearchEverywhereTab::Symbols);
        assert!(!app.search_everywhere_class_filter);
    }

    #[test]
    fn run_command_go_to_line_opens_the_dialog() {
        let mut app = app_without_gui();
        app.run_command(CommandAction::GoToLine, &egui::Context::default());
        assert!(app.show_go_to_line);
    }

    #[test]
    fn is_command_enabled_gates_find_in_path_on_project_and_others_on_active_tab() {
        let mut app = app_without_gui();
        assert!(!app.is_command_enabled(CommandAction::FindInPath));
        assert!(!app.is_command_enabled(CommandAction::Find));
        assert!(app.is_command_enabled(CommandAction::FindAction));

        app.new_untitled_tab();
        assert!(app.is_command_enabled(CommandAction::Find));

        let dir = tempfile::tempdir().unwrap();
        let project = Project::create(dir.path().join("proj")).unwrap();
        app.load_project(project, &egui::Context::default());
        assert!(app.is_command_enabled(CommandAction::FindInPath));
    }

    #[test]
    fn is_command_enabled_gates_replace_in_path_on_project_same_as_find_in_path() {
        let mut app = app_without_gui();
        assert!(!app.is_command_enabled(CommandAction::ReplaceInPath));

        let dir = tempfile::tempdir().unwrap();
        let project = Project::create(dir.path().join("proj")).unwrap();
        app.load_project(project, &egui::Context::default());
        assert!(app.is_command_enabled(CommandAction::ReplaceInPath));
    }

    #[test]
    fn run_command_replace_in_path_triggers_the_replace_flow() {
        let mut app = app_without_gui();
        app.bottom_view = BottomView::Problems;
        app.run_command(CommandAction::ReplaceInPath, &egui::Context::default());
        assert!(app.search_replace_open);
        assert_eq!(app.bottom_view, BottomView::Search);
    }

    #[test]
    fn run_command_find_action_opens_the_palette() {
        let mut app = app_without_gui();
        app.run_command(CommandAction::FindAction, &egui::Context::default());
        assert!(app.command_palette_open);
    }

    #[test]
    fn run_command_undo_delegates_to_the_active_tabs_buffer() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.tabs[0].buffer.insert(0, "x");
        assert_eq!(app.tabs[0].buffer.text(), "x");

        app.run_command(CommandAction::Undo, &egui::Context::default());

        assert_eq!(app.tabs[0].buffer.text(), "");
    }

    // ---- A6: code folding ----

    #[test]
    fn run_command_collapse_fold_collapses_the_range_at_the_caret() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.tabs[0].buffer.set_syntax(Some(&ide_core::RUST));
        app.tabs[0].buffer.insert(0, "fn f() {\n    a();\n}\n");
        app.active_cursor_offset = Some(0);

        app.run_command(CommandAction::CollapseFold, &egui::Context::default());

        assert!(app.tabs[0].editor.is_folded(0));
    }

    #[test]
    fn run_command_collapse_fold_reveals_a_caret_it_just_hid() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.tabs[0].buffer.set_syntax(Some(&ide_core::RUST));
        app.tabs[0].buffer.insert(0, "fn f() {\n    a();\n}\n");
        let body_offset = app.tabs[0].buffer.text().find("a()").unwrap();
        app.tabs[0]
            .buffer
            .text_buffer_mut()
            .set_selections(ide_core::Selections::single(ide_core::Selection::caret(
                body_offset,
            )));
        app.active_cursor_offset = Some(0);

        app.run_command(CommandAction::CollapseFold, &egui::Context::default());

        let head = app.tabs[0].buffer.text_buffer().selections().primary().head;
        assert_eq!(app.tabs[0].buffer.text_buffer().lines().line_at(head), 0);
    }

    #[test]
    fn run_command_expand_fold_uncollapses_the_range_at_the_caret() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.tabs[0].buffer.set_syntax(Some(&ide_core::RUST));
        app.tabs[0].buffer.insert(0, "fn f() {\n    a();\n}\n");
        app.tabs[0].editor.toggle_fold(0);
        app.active_cursor_offset = Some(0);

        app.run_command(CommandAction::ExpandFold, &egui::Context::default());

        assert!(!app.tabs[0].editor.is_folded(0));
    }

    #[test]
    fn run_command_collapse_all_folds_collapses_every_range() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.tabs[0].buffer.set_syntax(Some(&ide_core::RUST));
        app.tabs[0]
            .buffer
            .insert(0, "fn f() {\n    a();\n}\nfn g() {\n    b();\n}\n");

        app.run_command(CommandAction::CollapseAllFolds, &egui::Context::default());

        assert!(app.tabs[0].editor.is_folded(0));
        assert!(app.tabs[0].editor.is_folded(3));
    }

    #[test]
    fn run_command_expand_all_folds_clears_every_collapsed_range() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.tabs[0].buffer.set_syntax(Some(&ide_core::RUST));
        app.tabs[0].buffer.insert(0, "fn f() {\n    a();\n}\n");
        app.tabs[0].editor.toggle_fold(0);

        app.run_command(CommandAction::ExpandAllFolds, &egui::Context::default());

        assert!(!app.tabs[0].editor.is_folded(0));
    }

    #[test]
    fn fold_commands_are_enabled_only_with_an_active_tab() {
        let app = app_without_gui();
        assert!(app.active_tab.is_none());
        assert!(!app.is_command_enabled(CommandAction::CollapseFold));
        assert!(!app.is_command_enabled(CommandAction::ExpandFold));
        assert!(!app.is_command_enabled(CommandAction::CollapseAllFolds));
        assert!(!app.is_command_enabled(CommandAction::ExpandAllFolds));
    }

    #[test]
    fn run_command_go_to_declaration_dispatches_to_the_trigger_method() {
        let mut app = app_without_gui();
        app.run_command(CommandAction::GoToDeclaration, &egui::Context::default());
        assert_eq!(app.goto_action, Some(GotoKind::Definition));
    }

    #[test]
    fn run_command_go_to_implementation_dispatches_to_the_trigger_method() {
        let mut app = app_without_gui();
        app.run_command(CommandAction::GoToImplementation, &egui::Context::default());
        assert_eq!(app.goto_action, Some(GotoKind::Implementation));
    }

    #[test]
    fn run_command_go_to_type_declaration_dispatches_to_the_trigger_method() {
        let mut app = app_without_gui();
        app.run_command(
            CommandAction::GoToTypeDeclaration,
            &egui::Context::default(),
        );
        assert_eq!(app.goto_action, Some(GotoKind::TypeDefinition));
    }

    #[test]
    fn run_command_navigate_back_and_forward_dispatch_to_nav_back_and_forward() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();

        let mut app = app_without_gui();
        app.open_file(&a);
        app.push_nav_location();
        app.open_file(&b);
        app.push_nav_location();

        app.run_command(CommandAction::NavigateBack, &egui::Context::default());
        assert_eq!(app.active_tab, Some(0));

        app.run_command(CommandAction::NavigateForward, &egui::Context::default());
        assert_eq!(app.active_tab, Some(1));
    }

    // ---- G2: keymap overlay ----

    #[test]
    fn keymap_filtered_ids_with_empty_query_returns_every_command() {
        let app = app_without_gui();
        assert_eq!(app.keymap_filtered_ids().len(), command::commands().len());
    }

    #[test]
    fn keymap_filtered_ids_matches_title_case_insensitively() {
        let mut app = app_without_gui();
        app.keymap_search = "save".to_string();
        assert_eq!(
            app.keymap_filtered_ids(),
            vec!["SaveAll", "ToggleFormatOnSave"]
        );
    }

    #[test]
    fn keymap_filtered_ids_also_matches_the_effective_binding_label() {
        let mut app = app_without_gui();
        app.keymap_search = "shift+z".to_string();
        assert_eq!(app.keymap_filtered_ids(), vec!["Redo"]);
    }

    #[test]
    fn start_keymap_capture_sets_target_and_clears_stale_pending() {
        let mut app = app_without_gui();
        app.keymap_capture_pending = Some((KeyChord::new(egui::Key::S).command(), vec![]));

        app.start_keymap_capture("Undo");

        assert_eq!(app.keymap_capture_target, Some("Undo"));
        assert!(app.keymap_capture_pending.is_none());
    }

    #[test]
    fn confirm_keymap_capture_commits_the_pending_chord_and_clears_state() {
        let mut app = app_without_gui();
        app.start_keymap_capture("Undo");
        let chord = KeyChord::new(egui::Key::F9).command();
        app.keymap_capture_pending = Some((chord, vec![]));

        app.confirm_keymap_capture();

        assert_eq!(
            app.keymap.effective_binding("Undo").map(|b| b.mac),
            Some(chord)
        );
        assert!(app.keymap_capture_target.is_none());
        assert!(app.keymap_capture_pending.is_none());
    }

    #[test]
    fn confirm_keymap_capture_is_a_noop_with_nothing_pending() {
        let mut app = app_without_gui();
        app.start_keymap_capture("Undo");

        app.confirm_keymap_capture();

        assert!(app.keymap.effective_binding("Undo").is_some());
        assert!(!app.keymap.is_customized("Undo"));
    }

    #[test]
    fn cancel_keymap_capture_clears_state_without_committing() {
        let mut app = app_without_gui();
        app.start_keymap_capture("Undo");
        app.keymap_capture_pending = Some((KeyChord::new(egui::Key::F9).command(), vec![]));

        app.cancel_keymap_capture();

        assert!(app.keymap_capture_target.is_none());
        assert!(app.keymap_capture_pending.is_none());
        assert!(!app.keymap.is_customized("Undo"));
    }

    #[test]
    fn reset_keymap_binding_clears_a_customization() {
        let mut app = app_without_gui();
        app.keymap.set_override("Undo", None);
        assert!(app.keymap.is_customized("Undo"));

        app.reset_keymap_binding("Undo");

        assert!(!app.keymap.is_customized("Undo"));
    }

    #[test]
    fn export_then_import_keymap_round_trips_through_a_real_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keymap.txt");
        let mut app = app_without_gui();
        app.keymap.set_override("ShowUsages", None);

        app.export_keymap_to(&path).unwrap();

        let mut reimported = app_without_gui();
        let report = reimported.import_keymap_from(&path).unwrap();

        assert!(report.skipped_unknown_ids.is_empty());
        assert_eq!(reimported.keymap.effective_binding("ShowUsages"), None);
    }

    #[test]
    fn import_keymap_from_a_missing_path_returns_an_error() {
        let mut app = app_without_gui();
        let result = app.import_keymap_from(Path::new("/nonexistent/keymap.txt"));
        assert!(result.is_err());
    }

    /// Drives a real `egui::Context` through one pass with `S` pressed
    /// under Cmd, mirroring `command.rs`'s own `pressed_after` test helper
    /// -- proves `poll_keymap_capture` actually reads live input, not a
    /// mock, and that `is_bare_modifier_key` doesn't block a real key.
    #[test]
    fn poll_keymap_capture_captures_a_real_keypress() {
        let mut app = app_without_gui();
        app.start_keymap_capture("Undo");
        let modifiers = egui::Modifiers {
            command: true,
            ..egui::Modifiers::NONE
        };
        let ctx = egui::Context::default();
        ctx.begin_pass(egui::RawInput {
            events: vec![
                egui::Event::ModifiersChanged(modifiers),
                egui::Event::Key {
                    key: egui::Key::S,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers,
                },
            ],
            ..Default::default()
        });

        app.poll_keymap_capture(&ctx);

        let (chord, _) = app.keymap_capture_pending.unwrap();
        assert_eq!(chord.key, egui::Key::S);
        assert!(chord.modifiers.command);
    }

    /// A bare modifier key-down event (e.g. the physical Cmd key on its
    /// own) must not itself be captured as the chord's key (`keymap.md`
    /// §2.6) -- without `is_bare_modifier_key`'s exclusion this would
    /// capture a nonsensical binding that can never fire again.
    #[test]
    fn poll_keymap_capture_ignores_a_bare_modifier_press() {
        let mut app = app_without_gui();
        app.start_keymap_capture("Undo");
        let modifiers = egui::Modifiers {
            command: true,
            ..egui::Modifiers::NONE
        };
        let ctx = egui::Context::default();
        ctx.begin_pass(egui::RawInput {
            events: vec![
                egui::Event::ModifiersChanged(modifiers),
                egui::Event::Key {
                    key: egui::Key::SuperLeft,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers,
                },
            ],
            ..Default::default()
        });

        app.poll_keymap_capture(&ctx);

        assert!(app.keymap_capture_pending.is_none());
        assert_eq!(app.keymap_capture_target, Some("Undo"));
    }

    // ---- B2: fleet shell ----

    #[test]
    fn smart_mode_state_is_off_with_no_active_language() {
        let app = app_without_gui();
        assert_eq!(app.smart_mode_state(), SmartModeState::Off);
    }

    #[test]
    fn smart_mode_state_is_error_when_the_server_failed_to_start() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.go"), "package main").unwrap();
        let mut app = app_without_gui();
        app.open_project(dir.path(), &egui::Context::default());
        app.custom_languages = vec![go_config()];
        wait_until(|| app.poll_tree_scan());

        // Deterministic: `go_config`'s command doesn't exist.
        assert_eq!(app.smart_mode_state(), SmartModeState::Error);
    }

    #[test]
    fn toggle_smart_mode_off_or_error_attempts_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.go"), "package main").unwrap();
        let mut app = app_without_gui();
        app.open_project(dir.path(), &egui::Context::default());
        app.custom_languages = vec![go_config()];
        wait_until(|| app.poll_tree_scan());
        assert_eq!(app.smart_mode_state(), SmartModeState::Error);

        app.lsp.server_error = None;
        app.toggle_smart_mode();

        assert!(app.lsp.is_running() || app.lsp.server_error.is_some());
    }

    #[test]
    fn toggle_smart_mode_with_no_active_language_is_a_harmless_noop() {
        let mut app = app_without_gui();
        app.toggle_smart_mode();
        assert!(!app.lsp.is_running());
    }

    #[test]
    fn toggle_tool_window_flips_project_and_claude_independently() {
        let mut app = app_without_gui();
        assert!(app.show_project_tool_window);
        assert!(app.show_claude_tool_window);

        app.toggle_tool_window(ToolWindow::Project);
        assert!(!app.show_project_tool_window);
        assert!(app.show_claude_tool_window);

        app.toggle_tool_window(ToolWindow::Claude);
        assert!(!app.show_claude_tool_window);
    }

    #[test]
    fn toggle_bottom_tool_window_forces_open_and_switches_view() {
        let mut app = app_without_gui();
        app.show_bottom_tool_window = false;
        app.bottom_view = BottomView::Problems;

        app.toggle_bottom_tool_window(BottomView::Search);

        assert!(app.show_bottom_tool_window);
        assert_eq!(app.bottom_view, BottomView::Search);
    }

    #[test]
    fn toggle_bottom_tool_window_switches_view_without_closing_when_a_different_tab_is_up() {
        let mut app = app_without_gui();
        app.show_bottom_tool_window = true;
        app.bottom_view = BottomView::Problems;

        app.toggle_bottom_tool_window(BottomView::CargoOutput);

        assert!(app.show_bottom_tool_window);
        assert_eq!(app.bottom_view, BottomView::CargoOutput);
    }

    #[test]
    fn toggle_bottom_tool_window_closes_when_its_own_tab_is_already_the_visible_one() {
        let mut app = app_without_gui();
        app.show_bottom_tool_window = true;
        app.bottom_view = BottomView::Problems;

        app.toggle_bottom_tool_window(BottomView::Problems);

        assert!(!app.show_bottom_tool_window);
        // Reopening keeps whatever tab was last selected (§3.7).
        assert_eq!(app.bottom_view, BottomView::Problems);
    }

    #[test]
    fn toggle_zen_mode_flips_the_flag_without_touching_tool_window_flags() {
        let mut app = app_without_gui();
        app.show_claude_tool_window = false;

        app.toggle_zen_mode();
        assert!(app.zen_mode);
        app.toggle_zen_mode();
        assert!(!app.zen_mode);

        assert!(!app.show_claude_tool_window);
    }

    #[test]
    fn push_nav_location_is_a_noop_with_no_active_tab() {
        let mut app = app_without_gui();
        app.push_nav_location();
        assert!(!app.nav.can_go_back());
    }

    #[test]
    fn push_nav_location_is_a_noop_for_an_untitled_tab() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.push_nav_location();
        assert!(!app.nav.can_go_back());
    }

    #[test]
    fn open_file_then_open_diagnostic_coalesces_into_one_nav_entry() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "line one\nline two\n").unwrap();

        let mut app = app_without_gui();
        // Establish a first location so the same-file coalescing path (not
        // the "first push ever" path) is what's under test.
        app.open_file(&file);
        app.push_nav_location();
        app.open_diagnostic(
            &file,
            Position {
                line: 1,
                character: 0,
            },
        );

        assert!(!app.nav.can_go_back());
        assert!(!app.nav.can_go_forward());
    }

    #[test]
    fn nav_back_and_forward_round_trip_without_erasing_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();

        let mut app = app_without_gui();
        app.open_file(&a);
        app.push_nav_location();
        app.open_file(&b);
        app.push_nav_location();

        app.nav_back();
        assert_eq!(app.active_tab, Some(0)); // the `a` tab, opened first
                                             // nav_back must never itself push -- otherwise this would already
                                             // have erased the forward branch back to `b`.
        assert!(app.nav.can_go_forward());

        app.nav_forward();
        assert_eq!(app.active_tab, Some(1)); // the `b` tab, opened second
    }

    #[test]
    fn problems_count_aggregates_errors_and_warnings_across_files() {
        let mut app = app_without_gui();
        app.lsp.diagnostics.insert(
            PathBuf::from("a.rs"),
            vec![
                Diagnostic {
                    range: ide_lsp::Range {
                        start: Position {
                            line: 0,
                            character: 0,
                        },
                        end: Position {
                            line: 0,
                            character: 1,
                        },
                    },
                    severity: ide_lsp::DiagnosticSeverity::Error,
                    message: "e1".to_string(),
                },
                Diagnostic {
                    range: ide_lsp::Range {
                        start: Position {
                            line: 1,
                            character: 0,
                        },
                        end: Position {
                            line: 1,
                            character: 1,
                        },
                    },
                    severity: ide_lsp::DiagnosticSeverity::Warning,
                    message: "w1".to_string(),
                },
            ],
        );
        app.lsp.diagnostics.insert(
            PathBuf::from("b.rs"),
            vec![Diagnostic {
                range: ide_lsp::Range {
                    start: Position {
                        line: 0,
                        character: 0,
                    },
                    end: Position {
                        line: 0,
                        character: 1,
                    },
                },
                severity: ide_lsp::DiagnosticSeverity::Error,
                message: "e2".to_string(),
            }],
        );

        assert_eq!(app.problems_count(), (2, 1));
    }

    #[test]
    fn indent_label_formats_spaces_and_tabs() {
        assert_eq!(
            indent_label(IndentUnit {
                style: IndentStyle::Spaces,
                width: 4,
            }),
            "Spaces: 4"
        );
        assert_eq!(
            indent_label(IndentUnit {
                style: IndentStyle::Tabs,
                width: 8,
            }),
            "Tabs: 8"
        );
    }

    #[test]
    fn charset_label_defaults_to_utf8() {
        assert_eq!(charset_label(None), "UTF-8");
        assert_eq!(charset_label(Some(Charset::Utf8)), "UTF-8");
        assert_eq!(charset_label(Some(Charset::Latin1)), "Latin-1");
    }

    #[test]
    fn end_of_line_label_defaults_to_lf() {
        assert_eq!(end_of_line_label(None), "LF");
        assert_eq!(end_of_line_label(Some(EndOfLine::Crlf)), "CRLF");
    }

    #[test]
    fn is_command_enabled_gates_run_cargo_and_smart_mode() {
        let mut app = app_without_gui();
        assert!(!app.is_command_enabled(CommandAction::RunCargo(CargoCommand::Build)));
        assert!(!app.is_command_enabled(CommandAction::ToggleSmartMode));

        let dir = tempfile::tempdir().unwrap();
        let project = Project::create(dir.path().join("proj")).unwrap();
        std::fs::write(project.root().join("Cargo.toml"), "[package]").unwrap();
        app.load_project(project, &egui::Context::default());
        assert!(app.is_command_enabled(CommandAction::RunCargo(CargoCommand::Build)));
    }

    #[test]
    fn is_command_enabled_always_allows_tool_window_and_zen_toggles() {
        let app = app_without_gui();
        assert!(app.is_command_enabled(CommandAction::ToggleProjectToolWindow));
        assert!(app.is_command_enabled(CommandAction::ToggleClaudeToolWindow));
        assert!(app.is_command_enabled(CommandAction::ToggleZenMode));
        assert!(app.is_command_enabled(CommandAction::ShowKeymapSettings));
    }

    #[test]
    fn is_command_enabled_gates_tab_navigation_and_close_on_an_active_tab() {
        let mut app = app_without_gui();
        assert!(!app.is_command_enabled(CommandAction::NextTab));
        assert!(!app.is_command_enabled(CommandAction::PreviousTab));
        assert!(!app.is_command_enabled(CommandAction::CloseTab));

        app.new_untitled_tab();
        assert!(app.is_command_enabled(CommandAction::NextTab));
        assert!(app.is_command_enabled(CommandAction::PreviousTab));
        assert!(app.is_command_enabled(CommandAction::CloseTab));
    }

    #[test]
    fn run_command_next_and_previous_tab_dispatch_to_cycle_tab() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.new_untitled_tab();
        app.active_tab = Some(0);

        app.run_command(CommandAction::NextTab, &egui::Context::default());
        assert_eq!(app.active_tab, Some(1));

        app.run_command(CommandAction::PreviousTab, &egui::Context::default());
        assert_eq!(app.active_tab, Some(0));
    }

    #[test]
    fn run_command_close_tab_closes_the_active_tab() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.new_untitled_tab();
        app.active_tab = Some(1);

        app.run_command(CommandAction::CloseTab, &egui::Context::default());

        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.active_tab, Some(0));
    }

    #[test]
    fn run_command_toggle_zen_mode_flips_the_flag() {
        let mut app = app_without_gui();
        app.run_command(CommandAction::ToggleZenMode, &egui::Context::default());
        assert!(app.zen_mode);
    }

    #[test]
    fn run_command_toggle_find_tool_window_forces_it_open_on_search() {
        let mut app = app_without_gui();
        app.show_bottom_tool_window = false;

        app.run_command(
            CommandAction::ToggleFindToolWindow,
            &egui::Context::default(),
        );

        assert!(app.show_bottom_tool_window);
        assert_eq!(app.bottom_view, BottomView::Search);
    }

    #[test]
    fn run_command_toggle_vcs_tool_window_swaps_view_mode() {
        let mut app = app_without_gui();
        assert_eq!(app.view_mode, ViewMode::Editor);
        app.run_command(
            CommandAction::ToggleVcsToolWindow,
            &egui::Context::default(),
        );
        assert_eq!(app.view_mode, ViewMode::SourceControl);
    }

    // ---- file-structure-and-breadcrumbs ----

    #[test]
    fn active_document_symbols_with_no_active_tab_is_empty() {
        let app = app_without_gui();
        assert!(app.active_document_symbols().is_empty());
    }

    #[test]
    fn active_document_symbols_with_mismatched_path_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        app.lsp.document_symbols_path = Some(PathBuf::from("/other.rs"));
        app.lsp.document_symbols = vec![symbol(
            "Foo",
            SymbolKind::Struct,
            Path::new("/other.rs"),
            0,
            1,
        )];

        assert!(app.active_document_symbols().is_empty());
    }

    #[test]
    fn active_document_symbols_returns_the_active_tabs_own_symbols() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        let path = app.tabs[0].buffer.path().unwrap().to_path_buf();
        app.lsp.document_symbols_path = Some(path.clone());
        app.lsp.document_symbols = vec![symbol("main", SymbolKind::Function, &path, 0, 0)];

        assert_eq!(app.active_document_symbols().len(), 1);
        assert_eq!(app.active_document_symbols()[0].name, "main");
    }

    #[test]
    fn active_breadcrumbs_with_no_cursor_offset_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        let path = app.tabs[0].buffer.path().unwrap().to_path_buf();
        app.lsp.document_symbols_path = Some(path.clone());
        app.lsp.document_symbols = vec![symbol("main", SymbolKind::Function, &path, 0, 0)];

        assert!(app.active_breadcrumbs().is_empty());
    }

    #[test]
    fn active_breadcrumbs_returns_the_chain_containing_the_caret() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "struct Foo {\n    fn bar() {}\n}\n").unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        let path = app.tabs[0].buffer.path().unwrap().to_path_buf();
        app.lsp.document_symbols_path = Some(path.clone());
        app.lsp.document_symbols = vec![
            symbol("Foo", SymbolKind::Struct, &path, 0, 2),
            symbol("bar", SymbolKind::Function, &path, 1, 1),
        ];
        // Byte offset 13 is the very first character of line 1 (position
        // 1:0) -- inside both Foo's (0:0-2:0) and bar's (1:0-1:0) ranges.
        app.active_cursor_offset = Some(13);

        let chain = app.active_breadcrumbs();
        let names: Vec<&str> = chain.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["Foo", "bar"]);
    }

    #[test]
    fn trigger_file_structure_opens_and_resets_state() {
        let mut app = app_without_gui();
        app.file_structure_query = "stale".to_string();
        app.file_structure_selected = 3;

        app.trigger_file_structure();

        assert!(app.file_structure_open);
        assert!(app.file_structure_query.is_empty());
        assert_eq!(app.file_structure_selected, 0);
        assert!(app.pending_file_structure_focus);
    }

    #[test]
    fn close_file_structure_closes_it() {
        let mut app = app_without_gui();
        app.file_structure_open = true;

        app.close_file_structure();

        assert!(!app.file_structure_open);
    }

    #[test]
    fn file_structure_owns_escape_true_only_while_open() {
        let mut app = app_without_gui();
        assert!(!app.file_structure_owns_escape());
        app.file_structure_open = true;
        assert!(app.file_structure_owns_escape());
    }

    #[test]
    fn file_structure_move_selection_wraps() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn a() {}\nfn b() {}\n").unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        let path = app.tabs[0].buffer.path().unwrap().to_path_buf();
        app.lsp.document_symbols_path = Some(path.clone());
        app.lsp.document_symbols = vec![
            symbol("a", SymbolKind::Function, &path, 0, 0),
            symbol("b", SymbolKind::Function, &path, 1, 1),
        ];
        app.file_structure_selected = 0;

        app.file_structure_move_selection(-1);
        assert_eq!(app.file_structure_selected, 1);
        app.file_structure_move_selection(1);
        assert_eq!(app.file_structure_selected, 0);
    }

    #[test]
    fn file_structure_move_selection_with_no_rows_is_a_noop() {
        let mut app = app_without_gui();
        app.file_structure_selected = 0;
        app.file_structure_move_selection(1);
        assert_eq!(app.file_structure_selected, 0);
    }

    #[test]
    fn file_structure_confirm_jumps_and_closes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn a() {}\nfn b() {}\n").unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);
        let path = app.tabs[0].buffer.path().unwrap().to_path_buf();
        app.lsp.document_symbols_path = Some(path.clone());
        app.lsp.document_symbols = vec![
            symbol("a", SymbolKind::Function, &path, 0, 0),
            symbol("b", SymbolKind::Function, &path, 1, 1),
        ];
        app.file_structure_open = true;
        app.file_structure_selected = 1;

        app.file_structure_confirm();

        assert!(!app.file_structure_open);
        // Byte offset 10 is the start of line 1 ("fn b() {}").
        assert_eq!(app.pending_cursor_offset, Some(10));
    }

    #[test]
    fn file_structure_confirm_with_no_rows_is_a_noop() {
        let mut app = app_without_gui();
        app.file_structure_open = true;

        app.file_structure_confirm();

        assert!(app.file_structure_open);
    }

    #[test]
    fn sync_document_symbols_with_an_untitled_tab_does_not_panic() {
        let mut app = app_without_gui();
        app.new_untitled_tab();
        app.sync_document_symbols(0);
    }

    #[test]
    fn sync_document_symbols_with_a_real_path_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        let mut app = app_without_gui();
        app.open_file(&file);

        // No LSP client is running in this test harness -- must not panic
        // when `LspBridge::request_document_symbols` itself no-ops.
        app.sync_document_symbols(0);
        assert_eq!(
            app.document_symbols_requested_for.as_deref(),
            app.tabs[0].buffer.path()
        );
    }

    #[test]
    fn is_command_enabled_file_structure_requires_a_real_path() {
        let mut app = app_without_gui();
        assert!(!app.is_command_enabled(CommandAction::FileStructure));

        app.new_untitled_tab();
        assert!(!app.is_command_enabled(CommandAction::FileStructure));

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        app.open_file(&file);
        assert!(app.is_command_enabled(CommandAction::FileStructure));
    }

    #[test]
    fn run_command_file_structure_opens_the_popup() {
        let mut app = app_without_gui();
        app.run_command(CommandAction::FileStructure, &egui::Context::default());
        assert!(app.file_structure_open);
    }

    #[test]
    fn record_recent_file_moves_a_re_recorded_path_to_the_front_without_duplicating() {
        let mut app = app_without_gui();
        app.record_recent_file(PathBuf::from("a.rs"));
        app.record_recent_file(PathBuf::from("b.rs"));
        app.record_recent_file(PathBuf::from("a.rs"));

        assert_eq!(
            app.recent_files,
            vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")]
        );
    }

    #[test]
    fn record_recent_file_caps_at_max_recent_files() {
        let mut app = app_without_gui();
        for i in 0..(MAX_RECENT_FILES + 10) {
            app.record_recent_file(PathBuf::from(format!("f{i}.rs")));
        }

        assert_eq!(app.recent_files.len(), MAX_RECENT_FILES);
        assert_eq!(
            app.recent_files[0],
            PathBuf::from(format!("f{}.rs", MAX_RECENT_FILES + 9))
        );
    }

    #[test]
    fn open_file_records_a_freshly_opened_path_as_recent() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let canonical = std::fs::canonicalize(&file).unwrap();
        let mut app = app_without_gui();

        app.open_file(&file);

        assert_eq!(app.recent_files, vec![canonical]);
    }

    #[test]
    fn open_file_records_an_already_open_tabs_path_as_recent_again() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let canonical = std::fs::canonicalize(&file).unwrap();
        let mut app = app_without_gui();
        app.open_file(&file);
        app.record_recent_file(PathBuf::from("other.rs"));

        app.open_file(&file);

        assert_eq!(app.recent_files, vec![canonical, PathBuf::from("other.rs")]);
    }

    #[test]
    fn load_project_settings_truncates_a_recent_files_array_past_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let ide_dir = root.join(".ide");
        std::fs::create_dir_all(&ide_dir).unwrap();
        let total = MAX_RECENT_FILES + 50;
        for i in 0..total {
            std::fs::write(root.join(format!("f{i}.rs")), "x").unwrap();
        }
        let entries: Vec<String> = (0..total).map(|i| format!(r#""f{i}.rs""#)).collect();
        let json = format!(r#"{{"recent_files":[{}]}}"#, entries.join(","));
        std::fs::write(ide_dir.join("workspace.json"), json).unwrap();

        let mut app = app_without_gui();
        app.load_project_settings(&root, &egui::Context::default());

        assert_eq!(app.recent_files.len(), MAX_RECENT_FILES);
    }

    #[test]
    fn flush_and_load_project_settings_round_trips_recent_files_as_root_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::write(root.join("a.rs"), "fn a() {}").unwrap();
        std::fs::write(root.join("b.rs"), "fn b() {}").unwrap();
        let mut app = app_without_gui();
        app.open_project(&root, &egui::Context::default());
        app.open_file(&root.join("a.rs"));
        app.open_file(&root.join("b.rs"));

        app.flush_project_settings(&root);

        let raw = std::fs::read_to_string(root.join(".ide/workspace.json")).unwrap();
        assert!(raw.contains("a.rs"));
        assert!(!raw.contains(root.display().to_string().as_str()));

        let mut reloaded = app_without_gui();
        reloaded.load_project_settings(&root, &egui::Context::default());
        assert_eq!(
            reloaded.recent_files,
            vec![root.join("b.rs"), root.join("a.rs")]
        );
    }

    #[test]
    fn trigger_recent_files_opens_and_closes_recent_locations() {
        let mut app = app_without_gui();
        app.recent_locations_open = true;

        app.trigger_recent_files();

        assert!(app.recent_files_open);
        assert!(!app.recent_locations_open);
        assert!(app.pending_recent_files_focus);
        assert_eq!(app.recent_files_selected, 0);
    }

    #[test]
    fn trigger_recent_locations_opens_and_closes_recent_files() {
        let mut app = app_without_gui();
        app.recent_files_open = true;

        app.trigger_recent_locations();

        assert!(app.recent_locations_open);
        assert!(!app.recent_files_open);
        assert_eq!(app.recent_locations_selected, 0);
    }

    #[test]
    fn recent_files_rows_with_empty_query_is_mru_order_verbatim() {
        let mut app = app_without_gui();
        app.recent_files = vec![PathBuf::from("b.rs"), PathBuf::from("a.rs")];

        assert_eq!(
            app.recent_files_rows(),
            vec![PathBuf::from("b.rs"), PathBuf::from("a.rs")]
        );
    }

    #[test]
    fn recent_files_rows_filters_by_project_relative_path_not_absolute_path() {
        // Regression test: the project root itself often lives under a
        // directory whose name matches ordinary query letters (e.g. a
        // macOS temp dir like `/var/folders/.../T/tmpXYZ/`); scoring
        // against the absolute path would let that segment spuriously
        // match every entry, exactly the bug `tui-recent-files-and-
        // bookmarks.md` §7.1 already hit and fixed for the other frontend.
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let mut app = app_without_gui();
        app.open_project(&root, &egui::Context::default());
        app.recent_files = vec![root.join("src/main.rs")];
        app.recent_files_query = "zzz-not-in-project-relative-path".to_string();

        // The temp dir's own absolute path is virtually guaranteed not to
        // contain this literal marker, so a match here would prove
        // scoring leaked in the absolute prefix.
        assert!(app.recent_files_rows().is_empty());

        app.recent_files_query = "main".to_string();
        assert_eq!(app.recent_files_rows(), vec![root.join("src/main.rs")]);
    }

    #[test]
    fn recent_files_move_selection_clamps_instead_of_wrapping() {
        let mut app = app_without_gui();
        app.recent_files = vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")];

        app.recent_files_move_selection(-1);
        assert_eq!(app.recent_files_selected, 0);

        app.recent_files_move_selection(1);
        app.recent_files_move_selection(1);
        assert_eq!(app.recent_files_selected, 1);
    }

    #[test]
    fn recent_files_confirm_opens_the_selected_row_without_moving_the_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let canonical = std::fs::canonicalize(&file).unwrap();
        let mut app = app_without_gui();
        app.recent_files = vec![canonical.clone()];
        app.recent_files_open = true;
        app.recent_files_selected = 0;
        app.pending_cursor_offset = None;

        app.recent_files_confirm();

        assert!(!app.recent_files_open);
        assert_eq!(
            app.active_tab.and_then(|i| app.tabs[i].buffer.path()),
            Some(canonical.as_path())
        );
        assert_eq!(app.pending_cursor_offset, None);
    }

    #[test]
    fn recent_locations_rows_reflects_nav_history_most_recent_first() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "one\ntwo\nthree\n").unwrap();
        let mut app = app_without_gui();
        app.nav.push(NavLocation {
            path: file.clone(),
            offset: 0,
        });
        app.nav.push(NavLocation {
            path: file.clone(),
            offset: 4,
        });

        let rows = app.recent_locations_rows();

        assert_eq!(rows.len(), 1);
        let (location, line, preview) = &rows[0];
        assert_eq!(location.path, file);
        assert_eq!(*line, Some(2));
        assert_eq!(preview.as_deref(), Some("two"));
    }

    #[test]
    fn recent_locations_rows_is_unavailable_for_a_since_deleted_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("gone.rs");
        std::fs::write(&file, "x").unwrap();
        let mut app = app_without_gui();
        app.nav.push(NavLocation {
            path: file.clone(),
            offset: 0,
        });
        std::fs::remove_file(&file).unwrap();

        let rows = app.recent_locations_rows();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, None);
        assert_eq!(rows[0].2, None);
    }

    #[test]
    fn recent_locations_confirm_sets_pending_cursor_offset_and_does_not_push_nav_location() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, "one\ntwo\nthree\n").unwrap();
        let mut app = app_without_gui();
        app.nav.push(NavLocation {
            path: file.clone(),
            offset: 4,
        });
        app.recent_locations_open = true;
        app.recent_locations_selected = 0;
        let entries_before = app.recent_locations_rows().len();

        app.recent_locations_confirm();

        assert!(!app.recent_locations_open);
        assert_eq!(app.pending_cursor_offset, Some(4));
        assert_eq!(app.recent_locations_rows().len(), entries_before);
    }

    #[test]
    fn recent_locations_move_selection_clamps_instead_of_wrapping() {
        let dir = tempfile::tempdir().unwrap();
        let file_a = dir.path().join("a.rs");
        let file_b = dir.path().join("b.rs");
        std::fs::write(&file_a, "one\ntwo\n").unwrap();
        std::fs::write(&file_b, "one\ntwo\n").unwrap();
        let mut app = app_without_gui();
        app.nav.push(NavLocation {
            path: file_a,
            offset: 0,
        });
        app.nav.push(NavLocation {
            path: file_b,
            offset: 0,
        });

        app.recent_locations_move_selection(-1);
        assert_eq!(app.recent_locations_selected, 0);

        app.recent_locations_move_selection(1);
        app.recent_locations_move_selection(1);
        assert_eq!(app.recent_locations_selected, 1);
    }

    #[test]
    fn is_command_enabled_always_allows_recent_files_and_locations() {
        let app = app_without_gui();
        assert!(app.is_command_enabled(CommandAction::RecentFiles));
        assert!(app.is_command_enabled(CommandAction::RecentLocations));
    }

    #[test]
    fn run_command_recent_files_and_locations_open_their_popups() {
        let mut app = app_without_gui();
        app.run_command(CommandAction::RecentFiles, &egui::Context::default());
        assert!(app.recent_files_open);

        app.run_command(CommandAction::RecentLocations, &egui::Context::default());
        assert!(app.recent_locations_open);
        assert!(!app.recent_files_open);
    }
}
