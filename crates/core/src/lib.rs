//! ide-core: editor buffer, file/project model, git integration. Owned by
//! rust-core-dev.

pub mod buffer;
pub mod buffer_search;
pub mod editorconfig;
pub mod file_watcher;
pub mod fuzzy;
pub mod git;
pub mod language;
pub mod project;
pub mod project_settings;
pub mod search;
pub mod search_in_path;
pub mod syntax;
pub mod text;
pub mod workspace_edit;

pub use buffer::{Buffer, BufferError};
pub use buffer_search::{
    find_matches, replace_all, replace_one, MatchResults, ReplaceResult, SearchOptions,
    SearchQuery, SearchQueryError, MAX_SEARCH_MATCHES,
};
pub use editorconfig::{
    Charset, EditorConfig, EditorConfigError, EndOfLine, MAX_EDITORCONFIG_BYTES,
    MAX_EDITORCONFIG_DEPTH, MAX_EDITORCONFIG_SECTIONS,
};
pub use file_watcher::{
    FileWatcher, WatchError, WatchEvent, DEBOUNCE_WINDOW, MAX_COALESCED_PATHS, SUPPRESS_WINDOW,
};
pub use fuzzy::{
    fuzzy_match_files, fuzzy_score, FuzzyFileMatch, FuzzyFileResults, FuzzyMatch,
    MAX_FUZZY_FILE_RESULTS,
};
pub use git::{
    diff_text, BlameLine, BranchInfo, ChangeKind, CommitDetail, CommitLogFilter, CommitNode,
    ConflictSides, DiffHunk, DiffLine, DiffSpan, FileDiff, GitError, GitRepo, MergeOutcome,
    StatusEntry, WorkingTreeStatus, WorktreeInfo, MAX_BLAME_LINES, MAX_DIFF_LINES,
};
pub use language::{
    detect_active_languages, detect_language, detect_language_suggestions, language_for_path,
    LanguageConfig, LanguageSuggestion,
};
pub use project::{DirEntry, DirEntryKind, Project, ProjectError};
pub use search::{
    search_tree, SearchMatch, SearchResults, MAX_SEARCHABLE_FILE_BYTES, MAX_SEARCH_RESULTS,
};
pub use search_in_path::{
    replace_in_path, search_tree_advanced, PathSearchError, PathSearchMatch, PathSearchOptions,
    PathSearchResults, ReplaceInPathResult,
};
pub use syntax::{
    syntax_for_extension, syntax_for_path, tokenize, tokenize_range, LineState, SyntaxRules, Token,
    TokenKind, C, CSS, DOCKERFILE, ENV, GO, INI, JAVA, JAVASCRIPT, JSON, MAKEFILE, MARKDOWN,
    MAX_HIGHLIGHTED_FILE_BYTES, PYTHON, RUST, SHELL, SQL, SYSTEMD_UNIT, TOML, XML, YAML,
};
pub use text::word_at;
pub use text::{
    all_occurrences, leading_whitespace, newline_indent, next_occurrence, splits_a_pair, Bias,
    BracketPair, Change, FoldKind, FoldRange, IndentStyle, IndentUnit, LineDirection, LineIndex,
    Selection, Selections, TextBuffer, Transaction, TransactionError, MAX_BRACKET_SCAN_BYTES,
    MAX_OCCURRENCES,
};
pub use workspace_edit::{
    apply_transaction, apply_workspace_edit_to_disk, FileEdit, WorkspaceEdit, WorkspaceEditError,
};
