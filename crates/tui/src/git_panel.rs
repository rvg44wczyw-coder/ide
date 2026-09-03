//! Source Control panel state/logic: commit graph, side-by-side diff,
//! three-way conflict resolution, staging/commit, branch operations, and
//! log filtering, all backed by `ide_core::GitRepo`. See `docs/features/
//! tui-git-panel.md` §2.1/§3 (T11, itself porting `docs/features/
//! git-support.md` §2.2/§3) and `docs/features/
//! tui-git-staging-branches-and-log-filters.md` §2.1/§3 (T28, itself
//! porting `git-commit-and-staging.md`, the branch half of
//! `git-branches-and-blame.md`, and `git-log-viewer.md`). Rendering lives
//! in `ui.rs` alongside the rest of `App`'s rendering; everything here is
//! plain state transitions, unit-testable without a terminal harness --
//! ported near-verbatim from `crates/ui/src/git_panel.rs`, which has zero
//! `egui`/`eframe` dependency itself.

use ide_core::{
    BranchInfo, CommitLogFilter, CommitNode, ConflictSides, FileDiff, GitError, GitRepo,
    MergeOutcome, WorkingTreeStatus, WorktreeInfo,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Matches `docs/features/git-support.md` §3's "up to a fixed cap (e.g.
/// 500)" commit-graph size.
pub const COMMIT_GRAPH_LIMIT: usize = 500;

pub struct ConflictResolutionState {
    pub path: PathBuf,
    pub sides: ConflictSides,
    /// Scratch buffer the user edits; pre-seeded from `sides.ours`.
    pub result: String,
}

impl ConflictResolutionState {
    fn new(path: PathBuf, sides: ConflictSides) -> Self {
        let result = sides.ours.clone().unwrap_or_default();
        Self {
            path,
            sides,
            result,
        }
    }
}

/// The branches popup's own transient UI state (`docs/features/
/// tui-git-staging-branches-and-log-filters.md` §2.1, porting
/// `git-branches-and-blame.md` §2.2.1) -- separate from `GitPanel`'s
/// always-empty-until-opened `branches` list, the same split `active_
/// conflict`/`conflicts` already keep.
#[derive(Default)]
pub struct BranchesPopupState {
    pub open: bool,
    /// Fuzzy-filter text, scored via `ide_core::fuzzy_score` against each
    /// branch name (`App::filtered_branch_rows`) -- persists across
    /// leaving `typing_filter` mode (`Esc` there only stops editing, the
    /// same "stop editing, not discard" convention every other text-entry
    /// state in this popup follows), cleared only when the whole popup
    /// closes.
    pub filter: String,
    /// `true` while `/` has put the popup into filter-typing mode
    /// (`docs/features/tui-git-staging-branches-and-log-filters.md`
    /// §3.4) -- gates `Char`/`Backspace` to editing `filter` instead of
    /// falling through to the `m`/`n`/`d` single-letter commands, the same
    /// text-entry-vs-command-conflict fix shape as `Message`/`Filter`
    /// focus elsewhere in this doc.
    pub typing_filter: bool,
    pub selected: usize,
    pub new_branch_name: String,
    pub show_new_branch_input: bool,
    /// Branch name pending a "not fully merged -- force delete?" confirm
    /// (`delete_branch`'s `Err(BranchNotMerged)` lands here instead of
    /// just being shown as an error) -- cleared on a successful delete,
    /// but left set on failure so the same confirm can retry with `force`
    /// without a second click/keypress chain.
    pub pending_delete: Option<String>,
}

/// The worktrees popup's own transient UI state (`docs/features/
/// git-worktrees.md` §2.2.1's `WorktreesPopupState`, adapted for
/// keyboard-only interaction, `docs/features/tui-git-worktrees.md` §2.1).
#[derive(Default)]
pub struct WorktreesPopupState {
    pub open: bool,
    pub worktrees: Vec<WorktreeInfo>,
    /// Row navigation. `ide-ui`'s own `WorktreesPopupState` has no such
    /// field -- each row carries its own click targets there. `ide-tui`
    /// is keyboard-only, so it needs an index the same way
    /// `branches_popup.selected` already does.
    pub selected: usize,
    /// `true` while the "Add worktree" form has focus (§2.2's `n` key) --
    /// `ide-tui`-specific: `ide-ui`'s three fields are always-visible
    /// inline text boxes with no modal "now typing" state to track.
    pub adding: bool,
    pub add_field: WorktreeAddField,
    pub new_name: String,
    pub new_path: String,
    /// Empty means "create a new branch named `new_name`" (`GitRepo::
    /// add_worktree`'s own `branch: None` case) -- surfaced as placeholder
    /// text while `add_field == Branch` and the field is still empty
    /// (§2.4), not a separate checkbox, exactly like `ide-ui`'s own form.
    pub new_branch: String,
    pub error: Option<String>,
    /// Set when a plain (`force: false`) `remove_worktree` call fails with
    /// `WorktreeHasUncommittedChanges` or `WorktreeLocked`, so the popup
    /// can offer a force-confirm instead of just showing the error --
    /// same two-step pattern `branches_popup.pending_delete` already uses.
    pub pending_force_remove: Option<String>,
}

/// Which of the "Add worktree" form's three fields is currently being
/// typed into. `Tab`/`Shift+Tab` cycle through all three in this order
/// (wrapping); this is a 3-way generalization of `DebugConfigField`'s
/// existing 2-way toggle (`tui-debugger.md` §2.5) -- the same shape, one
/// more variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorktreeAddField {
    #[default]
    Name,
    Path,
    Branch,
}

impl WorktreeAddField {
    pub fn next(self) -> Self {
        match self {
            Self::Name => Self::Path,
            Self::Path => Self::Branch,
            Self::Branch => Self::Name,
        }
    }
    pub fn prev(self) -> Self {
        match self {
            Self::Name => Self::Branch,
            Self::Path => Self::Name,
            Self::Branch => Self::Path,
        }
    }
}

/// The log viewer's own filter-bar state (`docs/features/
/// tui-git-staging-branches-and-log-filters.md` §2.1, porting
/// `git-log-viewer.md` §2.2) -- kept separate from `GitPanel::graph`
/// itself (which stays "last query's result set").
#[derive(Default)]
pub struct LogFilterState {
    pub branch: String,
    pub author: String,
    pub path: String,
    /// Free-typed date bounds (`YYYY-MM-DD`); parsed to Unix-seconds
    /// bounds only when applying the filter -- kept as raw text here so a
    /// partially-typed date doesn't get silently discarded mid-edit.
    pub since: String,
    pub until: String,
    pub query: String,
    /// Set when applying produced a `GitError` (an unresolvable `branch`,
    /// or an unparsable `since`/`until`). The graph is left as whatever it
    /// last successfully was, not cleared, on a failed apply.
    pub error: Option<String>,
    /// `true` while `GitPanel::graph` holds a `file_history` result
    /// instead of a `commit_graph` result -- the filter bar is hidden
    /// while this is set and a "back to log" affordance takes its place.
    pub viewing_file_history: Option<PathBuf>,
}

#[derive(Default)]
pub struct GitPanel {
    repo: Option<GitRepo>,
    pub graph: Vec<CommitNode>,
    pub selected_commit: Option<String>,
    /// Diff currently shown: a commit's changed files (`diff_commit`), or
    /// a single-element `Vec` for the active tab's working-tree diff
    /// (`diff_file`) when no commit is selected.
    pub diff: Option<Vec<FileDiff>>,
    pub conflicts: Vec<PathBuf>,
    pub active_conflict: Option<ConflictResolutionState>,
    /// Set instead of `active_conflict` when `conflict_sides()` errors for
    /// a selected path (non-UTF-8/binary side) -- the UI shows a
    /// placeholder message for this specific path and offers no Resolve
    /// UI, rather than the three-way panel.
    pub binary_conflict: Option<PathBuf>,
    /// Cached at `refresh()` time, same pattern as `graph`/`conflicts`.
    pub current_branch: Option<String>,
    pub status: WorkingTreeStatus,
    pub commit_message: String,
    pub amend: bool,
    /// A path awaiting a user's confirm/cancel on Discard -- distinct
    /// from the editor's unrelated "discard unsaved tab changes" modal.
    pub pending_discard: Option<PathBuf>,
    /// Loaded by `open_branches_popup` and refreshed after every mutating
    /// branch operation -- not eagerly loaded by `refresh()` itself,
    /// matching `ide-ui`'s own laziness (a manual reset shouldn't pay for
    /// a branch listing nobody's currently looking at).
    pub branches: Vec<BranchInfo>,
    pub branches_popup: BranchesPopupState,
    /// Loaded by `open_worktrees_popup` and refreshed after every mutating
    /// worktree operation -- not eagerly loaded by `refresh()` itself,
    /// same laziness as `branches`/`branches_popup`.
    pub worktrees_popup: WorktreesPopupState,
    /// `true` between a `merge_branch` call that returned `Conflicts(_)`
    /// and the resulting commit actually landing -- purely a UI label/
    /// default-message concern. Cleared the moment `commit()` succeeds,
    /// and reset to `false` by `refresh()` alongside `active_conflict`/
    /// `pending_discard`.
    pub merging: bool,
    pub log_filter: LogFilterState,
}

impl GitPanel {
    pub fn is_repo(&self) -> bool {
        self.repo.is_some()
    }

    /// (Re-)opens the repository at `project_root` and reloads the graph
    /// and conflicts list. Not being a repo is not an error -- it just
    /// clears all git state (doc §3's "not a repository" message). Called
    /// exactly once, from `App::new` -- `ide-tui` has no refresh/reload
    /// action of any kind (`docs/features/tui-git-panel.md` §1), unlike
    /// `ide-ui`'s equivalent, which re-runs this on its own tree-refresh
    /// action.
    pub fn refresh(&mut self, project_root: &Path) {
        match GitRepo::open(project_root) {
            Ok(repo) => {
                self.graph = repo
                    .commit_graph(COMMIT_GRAPH_LIMIT, &CommitLogFilter::default())
                    .unwrap_or_default();
                self.conflicts = repo.conflicts().unwrap_or_default();
                self.current_branch = repo.current_branch();
                self.status = repo.status().unwrap_or_default();
                self.repo = Some(repo);
            }
            Err(_) => {
                self.repo = None;
                self.graph.clear();
                self.conflicts.clear();
                self.current_branch = None;
                self.status = WorkingTreeStatus::default();
            }
        }
        self.selected_commit = None;
        self.diff = None;
        self.active_conflict = None;
        self.binary_conflict = None;
        self.commit_message.clear();
        self.amend = false;
        self.pending_discard = None;
        self.merging = false;
        // `branches` is deliberately not reloaded here -- it stays lazily
        // loaded only by `open_branches_popup`, matching `ide-ui`'s own
        // laziness (`branches` field doc comment above).
        self.log_filter = LogFilterState::default();
    }

    /// Selects a commit from `graph` and loads its diff. `commit_id` must
    /// come from a `CommitNode` already in `graph` (same path-provenance
    /// discipline as `select_conflict` below).
    pub fn select_commit(&mut self, commit_id: &str) {
        let Some(repo) = &self.repo else { return };
        self.diff = repo.diff_commit(commit_id).ok();
        self.selected_commit = Some(commit_id.to_string());
    }

    /// Loads the working-tree diff for `absolute_path` (e.g. the active
    /// editor tab's path) into `diff`, converting to a repo-relative path
    /// by stripping `workdir()` as a prefix -- never any other path
    /// construction. A path outside the repo, or no diff (unchanged/
    /// untracked/binary), clears `diff` rather than erroring.
    /// Canonicalizes `absolute_path` first (`workdir()` is always
    /// canonical, so an uncanonicalized input would otherwise fail the
    /// prefix strip on any platform where the path involves a symlink).
    pub fn show_working_tree_diff(&mut self, absolute_path: &Path) {
        let Some(repo) = &self.repo else {
            self.diff = None;
            return;
        };
        self.diff = std::fs::canonicalize(absolute_path)
            .ok()
            .and_then(|canonical| {
                canonical
                    .strip_prefix(repo.workdir())
                    .ok()
                    .map(|p| p.to_path_buf())
            })
            .and_then(|rel| repo.diff_file(rel).ok().flatten())
            .map(|d| vec![d]);
    }

    /// Same canonicalize + `strip_prefix` conversion `show_working_tree_
    /// diff`/`blame_for` already use -- independent of whatever `self.diff`
    /// currently shows. Empty with no repo, an untracked path, or no diff
    /// (`docs/features/tui-git-gutter.md` §2.2).
    pub fn hunks_for(&self, absolute_path: &Path) -> Vec<ide_core::DiffHunk> {
        let Some(repo) = &self.repo else {
            return Vec::new();
        };
        std::fs::canonicalize(absolute_path)
            .ok()
            .and_then(|canonical| {
                canonical
                    .strip_prefix(repo.workdir())
                    .ok()
                    .map(|p| p.to_path_buf())
            })
            .and_then(|rel| repo.diff_file(rel).ok().flatten())
            .map(|d| d.hunks)
            .unwrap_or_default()
    }

    pub fn gutter_marks_for(&self, absolute_path: &Path) -> Vec<crate::git_gutter::GutterMark> {
        crate::git_gutter::marks_from_hunks(&self.hunks_for(absolute_path))
    }

    /// Selects a conflicted path -- must come from `conflicts` (never any
    /// other source) -- and loads its sides. Sets `binary_conflict`
    /// instead of `active_conflict` if `conflict_sides()` errors
    /// (non-UTF-8/binary side).
    pub fn select_conflict(&mut self, path: &Path) {
        self.active_conflict = None;
        self.binary_conflict = None;
        let Some(repo) = &self.repo else { return };
        match repo.conflict_sides(path) {
            Ok(sides) => {
                self.active_conflict =
                    Some(ConflictResolutionState::new(path.to_path_buf(), sides));
            }
            Err(_) => self.binary_conflict = Some(path.to_path_buf()),
        }
    }

    /// Overwrites the Result area with the "ours" side (or clears it, if
    /// that side is `None` -- a delete). No-op with no active conflict.
    pub fn accept_ours(&mut self) {
        if let Some(state) = &mut self.active_conflict {
            state.result = state.sides.ours.clone().unwrap_or_default();
        }
    }

    /// Overwrites the Result area with the "theirs" side (or clears it).
    pub fn accept_theirs(&mut self) {
        if let Some(state) = &mut self.active_conflict {
            state.result = state.sides.theirs.clone().unwrap_or_default();
        }
    }

    /// Clears `active_conflict`/`binary_conflict` without touching
    /// anything else -- `ide-tui`'s modal Esc-while-resolving handler
    /// (`docs/features/tui-git-panel.md` §2.1/§3.2). Not present in
    /// `ide-ui`, which never needed an explicit "stop resolving without
    /// picking a conflict" action since its UI just lets the user click a
    /// different list row directly.
    pub fn cancel_conflict(&mut self) {
        self.active_conflict = None;
        self.binary_conflict = None;
    }

    /// Calls `resolve_conflict(path, result)` for the active conflict,
    /// re-queries `conflicts()` on success, and clears the active-conflict
    /// panel. On failure, leaves `active_conflict` untouched (so the
    /// user's edit isn't lost) and returns the error for the caller to
    /// surface. No-op returning `Ok(())` if there's no active conflict or
    /// no open repository.
    pub fn mark_resolved(&mut self) -> Result<(), String> {
        let (Some(state), Some(repo)) = (&self.active_conflict, &self.repo) else {
            return Ok(());
        };
        repo.resolve_conflict(&state.path, &state.result)
            .map_err(|e| e.to_string())?;
        self.conflicts = repo.conflicts().unwrap_or_default();
        self.active_conflict = None;
        Ok(())
    }

    /// Refreshes `status` from the open repository. No-op with no repo. A
    /// transient git error mid-frame degrades to "nothing changed," not an
    /// error banner spamming every frame -- same permissive-on-error
    /// convention `refresh()`'s own graph/conflicts loads already use.
    pub fn sync_status(&mut self) {
        let Some(repo) = &self.repo else { return };
        if let Ok(status) = repo.status() {
            self.status = status;
        }
    }

    pub fn stage(&mut self, path: &Path) -> Result<(), String> {
        let Some(repo) = &self.repo else {
            return Ok(());
        };
        repo.stage_path(path).map_err(|e| e.to_string())?;
        self.sync_status();
        Ok(())
    }

    pub fn unstage(&mut self, path: &Path) -> Result<(), String> {
        let Some(repo) = &self.repo else {
            return Ok(());
        };
        repo.unstage_path(path).map_err(|e| e.to_string())?;
        self.sync_status();
        Ok(())
    }

    pub fn request_discard(&mut self, path: &Path) {
        self.pending_discard = Some(path.to_path_buf());
    }

    pub fn cancel_discard(&mut self) {
        self.pending_discard = None;
    }

    /// No-op `Ok(())` if there's nothing pending (mirrors `mark_resolved`'s
    /// "no active target" no-op). Clears `pending_discard` either way --
    /// an error still closes the modal, retrying a failed discard is a
    /// fresh attempt, not a modal that lingers on failure.
    pub fn confirm_discard(&mut self) -> Result<(), String> {
        let (Some(path), Some(repo)) = (self.pending_discard.take(), &self.repo) else {
            self.pending_discard = None;
            return Ok(());
        };
        let result = repo.discard_path(&path).map_err(|e| e.to_string());
        if result.is_ok() {
            self.sync_status();
        }
        result
    }

    /// No-op `Ok(())` if there's nothing to commit (mirrors the core
    /// layer's own empty-message rejection, checked here too).
    pub fn commit(&mut self) -> Result<(), String> {
        if self.commit_message.trim().is_empty() && !self.amend {
            return Ok(());
        }
        let Some(repo) = &self.repo else {
            return Ok(());
        };
        repo.commit(&self.commit_message, self.amend)
            .map_err(|e| e.to_string())?;
        self.commit_message.clear();
        self.amend = false;
        self.merging = false;
        self.sync_status();
        let repo = self.repo.as_ref().expect("checked above");
        self.graph = repo
            .commit_graph(COMMIT_GRAPH_LIMIT, &CommitLogFilter::default())
            .unwrap_or_default();
        self.log_filter.viewing_file_history = None;
        Ok(())
    }

    /// Rebuilds a `CommitLogFilter` from `log_filter`'s text fields and
    /// reloads `graph` via the two-argument `commit_graph`. On any parse/
    /// git error, sets `log_filter.error` and leaves `graph` untouched --
    /// never a half-applied filter silently showing the wrong graph.
    pub fn apply_log_filter(&mut self) {
        let Some(repo) = &self.repo else { return };
        let filter = match Self::build_log_filter(&self.log_filter) {
            Ok(filter) => filter,
            Err(message) => {
                self.log_filter.error = Some(message);
                return;
            }
        };
        match repo.commit_graph(COMMIT_GRAPH_LIMIT, &filter) {
            Ok(graph) => {
                self.graph = graph;
                self.log_filter.error = None;
                self.log_filter.viewing_file_history = None;
                self.selected_commit = None;
                self.diff = None;
            }
            Err(e) => self.log_filter.error = Some(e.to_string()),
        }
    }

    fn build_log_filter(state: &LogFilterState) -> Result<CommitLogFilter, String> {
        Ok(CommitLogFilter {
            branch: non_empty(&state.branch),
            author: non_empty(&state.author),
            path: non_empty(&state.path).map(PathBuf::from),
            since: parse_date_bound(&state.since, false)?,
            until: parse_date_bound(&state.until, true)?,
            query: non_empty(&state.query),
        })
    }

    /// Clears every `LogFilterState` field and reloads the unfiltered
    /// graph -- the "Clear Filter" action.
    pub fn clear_log_filter(&mut self) {
        self.log_filter = LogFilterState::default();
        self.apply_log_filter();
    }

    /// Loads `path`'s rename-aware history into `graph` via
    /// `GitRepo::file_history` and sets `log_filter.viewing_file_history`.
    /// `path` must already be repository-relative -- the caller strips the
    /// project root off the active tab's absolute path first.
    pub fn show_file_history(&mut self, path: &Path) {
        let Some(repo) = &self.repo else { return };
        self.graph = repo
            .file_history(path, COMMIT_GRAPH_LIMIT)
            .unwrap_or_default();
        self.log_filter.error = None;
        self.log_filter.viewing_file_history = Some(path.to_path_buf());
        self.selected_commit = None;
        self.diff = None;
    }

    /// Leaves file-history view and reloads the graph under whatever
    /// `LogFilterState` currently holds (not necessarily unfiltered).
    pub fn back_to_log(&mut self) {
        self.log_filter.viewing_file_history = None;
        self.apply_log_filter();
    }

    fn reload_branches(&mut self) {
        self.branches = self
            .repo
            .as_ref()
            .and_then(|r| r.branches().ok())
            .unwrap_or_default();
    }

    /// Loads the branch list fresh and opens the popup with a clean
    /// transient state. Defensively re-opens the repository if it somehow
    /// isn't loaded yet (mirrors `refresh`'s own not-a-repo handling)
    /// rather than showing an empty popup with no way to recover.
    pub fn open_branches_popup(&mut self, project_root: &Path) {
        if self.repo.is_none() {
            self.refresh(project_root);
        }
        self.reload_branches();
        self.branches_popup = BranchesPopupState {
            open: true,
            ..BranchesPopupState::default()
        };
    }

    pub fn close_branches_popup(&mut self) {
        self.branches_popup = BranchesPopupState::default();
    }

    /// Checks out `name` (safe-mode checkout) and closes the popup on
    /// success -- on error, the popup stays open so the caller can
    /// surface the git2 error text inline.
    pub fn checkout_branch(&mut self, project_root: &Path, name: &str) -> Result<(), String> {
        let Some(repo) = &self.repo else {
            return Ok(());
        };
        repo.switch_branch(name).map_err(|e| e.to_string())?;
        self.refresh(project_root);
        self.reload_branches();
        self.close_branches_popup();
        Ok(())
    }

    /// Creates `name` from `HEAD` and, if `checkout` is set, switches to
    /// it. The branch list and popup input are refreshed/cleared even if
    /// the follow-up checkout fails (the branch itself was still
    /// created), but the popup only closes when the whole operation
    /// succeeds.
    pub fn create_branch(
        &mut self,
        project_root: &Path,
        name: &str,
        checkout: bool,
    ) -> Result<(), String> {
        let Some(repo) = &self.repo else {
            return Ok(());
        };
        repo.create_branch(name, None).map_err(|e| e.to_string())?;
        let switch_result = if checkout {
            self.repo
                .as_ref()
                .expect("checked above")
                .switch_branch(name)
                .map_err(|e| e.to_string())
        } else {
            Ok(())
        };
        self.refresh(project_root);
        self.reload_branches();
        self.branches_popup.show_new_branch_input = false;
        self.branches_popup.new_branch_name.clear();
        if switch_result.is_ok() {
            self.close_branches_popup();
        }
        switch_result
    }

    /// Marks `name` as pending a delete confirm. Never called for the
    /// branch `HEAD` currently points at -- the caller must not offer
    /// Delete on that row at all.
    pub fn request_delete_branch(&mut self, name: &str) {
        self.branches_popup.pending_delete = Some(name.to_string());
    }

    pub fn cancel_delete_branch(&mut self) {
        self.branches_popup.pending_delete = None;
    }

    /// Attempts to delete the pending branch. On `BranchNotMerged` (or any
    /// other error), `pending_delete` is left set so the same confirm can
    /// retry with `force: true` without a second click/keypress chain --
    /// deliberately different from `confirm_discard`, which always clears
    /// its pending target since a failed discard has nothing further to
    /// escalate to.
    pub fn confirm_delete_branch(
        &mut self,
        project_root: &Path,
        force: bool,
    ) -> Result<(), String> {
        let Some(name) = self.branches_popup.pending_delete.clone() else {
            return Ok(());
        };
        let Some(repo) = &self.repo else {
            self.branches_popup.pending_delete = None;
            return Ok(());
        };
        let result = repo.delete_branch(&name, force).map_err(|e| e.to_string());
        if result.is_ok() {
            self.branches_popup.pending_delete = None;
            self.refresh(project_root);
            self.reload_branches();
        }
        result
    }

    /// Opens the worktrees popup: resets `worktrees_popup` to a fresh
    /// (but `open: true`) default *before* loading, then calls
    /// `refresh_worktrees` -- this order matters here specifically because
    /// (unlike `open_branches_popup`, where the loaded list lives in a
    /// `GitPanel`-level field the popup-state reset never touches)
    /// `worktrees_popup.worktrees` lives *inside* the struct being reset,
    /// so resetting after loading would silently discard what was just
    /// loaded. Defensively re-opens the repository if it somehow isn't
    /// loaded yet, same as `open_branches_popup`.
    pub fn open_worktrees_popup(&mut self, project_root: &Path) {
        if self.repo.is_none() {
            self.refresh(project_root);
        }
        self.worktrees_popup = WorktreesPopupState {
            open: true,
            ..WorktreesPopupState::default()
        };
        self.refresh_worktrees();
    }

    pub fn close_worktrees_popup(&mut self) {
        self.worktrees_popup = WorktreesPopupState::default();
    }

    /// Calls `GitRepo::worktrees`, populating `worktrees_popup.worktrees`
    /// on success or `worktrees_popup.error` on failure. Not eagerly
    /// called by `refresh()` itself -- same lazy-load reasoning
    /// `reload_branches` already documents.
    pub fn refresh_worktrees(&mut self) {
        let Some(repo) = &self.repo else {
            self.worktrees_popup.worktrees = Vec::new();
            return;
        };
        match repo.worktrees() {
            Ok(worktrees) => {
                self.worktrees_popup.worktrees = worktrees;
                self.worktrees_popup.error = None;
            }
            Err(e) => self.worktrees_popup.error = Some(e.to_string()),
        }
    }

    /// Creates a worktree from the popup's own `new_name`/`new_path`/
    /// `new_branch` fields (empty `new_branch` becomes `None`). On
    /// success, clears the form, exits `adding` mode, and refreshes the
    /// list; on failure, sets `error` and leaves the form fields as-is so
    /// the user can fix and retry rather than retype. Doesn't close the
    /// whole popup either way -- unlike `create_branch`, adding a worktree
    /// isn't a context switch.
    pub fn create_worktree(&mut self) {
        let Some(repo) = &self.repo else {
            return;
        };
        let branch = (!self.worktrees_popup.new_branch.is_empty())
            .then(|| self.worktrees_popup.new_branch.clone());
        let result = repo.add_worktree(
            &self.worktrees_popup.new_name,
            &self.worktrees_popup.new_path,
            branch.as_deref(),
        );
        match result {
            Ok(()) => {
                self.worktrees_popup.new_name.clear();
                self.worktrees_popup.new_path.clear();
                self.worktrees_popup.new_branch.clear();
                self.worktrees_popup.adding = false;
                self.worktrees_popup.error = None;
                self.refresh_worktrees();
            }
            Err(e) => self.worktrees_popup.error = Some(e.to_string()),
        }
    }

    /// Removes `name`. On a `WorktreeHasUncommittedChanges` or
    /// `WorktreeLocked` failure with `force: false`, sets
    /// `pending_force_remove` *instead of* `error` -- the popup's confirm
    /// step uses a fixed message, not the raw error text, same two-step
    /// pattern `confirm_delete_branch` already uses for `BranchNotMerged`.
    /// Any other failure (including a retry with `force: true` that still
    /// fails) surfaces as `error`.
    pub fn remove_worktree(&mut self, name: &str, force: bool) {
        let Some(repo) = &self.repo else {
            return;
        };
        match repo.remove_worktree(name, force) {
            Ok(()) => {
                self.worktrees_popup.pending_force_remove = None;
                self.worktrees_popup.error = None;
                self.refresh_worktrees();
            }
            Err(GitError::WorktreeHasUncommittedChanges(_) | GitError::WorktreeLocked(_))
                if !force =>
            {
                self.worktrees_popup.pending_force_remove = Some(name.to_string());
            }
            Err(e) => self.worktrees_popup.error = Some(e.to_string()),
        }
    }

    /// Starts a merge of `name` into the current branch. On `Conflicts`,
    /// refreshes state (which repopulates `conflicts` from the repo's own
    /// index) and sets `merging` plus a pre-filled default commit message,
    /// but leaves the popup open (the caller -- `App::handle_git_branches_
    /// key` -- closes it and redirects to conflict resolution itself, a
    /// deliberate `ide-tui`-side deviation from `ide-ui`'s "leave it open"
    /// behaviour documented in `tui-git-staging-branches-and-log-filters
    /// .md` §3.4). On `Merged`/`FastForward`/`UpToDate`, refreshes and
    /// closes the popup.
    pub fn merge_branch(&mut self, project_root: &Path, name: &str) -> Result<(), String> {
        let Some(repo) = &self.repo else {
            return Ok(());
        };
        let outcome = repo.merge_branch(name).map_err(|e| e.to_string())?;
        self.refresh(project_root);
        self.reload_branches();
        if matches!(outcome, MergeOutcome::Conflicts(_)) {
            self.merging = true;
            let current = self
                .current_branch
                .clone()
                .unwrap_or_else(|| "HEAD".to_string());
            self.commit_message = format!("Merge branch '{name}' into {current}");
        } else {
            self.close_branches_popup();
        }
        Ok(())
    }

    /// Canonicalizes `absolute_path`, strips the repo's workdir prefix,
    /// and blames the resulting repo-relative path -- returns an empty
    /// `Vec` (not an error) for no repo open, an untracked path, or any
    /// canonicalization failure, mirroring `ide-ui`'s own `blame_for`
    /// (`docs/features/tui-blame.md` §2.2).
    pub fn blame_for(&self, absolute_path: &Path) -> Vec<ide_core::BlameLine> {
        let Some(repo) = &self.repo else {
            return Vec::new();
        };
        std::fs::canonicalize(absolute_path)
            .ok()
            .and_then(|canonical| {
                canonical
                    .strip_prefix(repo.workdir())
                    .ok()
                    .map(|p| p.to_path_buf())
            })
            .and_then(|rel| repo.blame_file(rel).ok())
            .unwrap_or_default()
    }

    /// Sanitizes and length-caps every text field before returning --
    /// ports the sanitize-then-truncate fix from `docs/security-findings/
    /// git-branches-and-blame-ui-2026-09-01.md` (findings 1-2) at the
    /// same layer `ide-ui`'s own `GitPanel::commit_detail` applies it at,
    /// not in `ide_core` or the render call site (`docs/features/
    /// tui-blame.md` §2.2). Strip-then-truncate in that order --
    /// stripping after truncation could re-expose an unterminated
    /// override cut mid-sequence.
    pub fn commit_detail(&self, commit_id: &str) -> Result<ide_core::CommitDetail, String> {
        let Some(repo) = &self.repo else {
            return Err("no repository open".to_string());
        };
        let detail = repo.commit_detail(commit_id).map_err(|e| e.to_string())?;
        Ok(ide_core::CommitDetail {
            summary: crate::blame_gutter::truncate_display(
                &crate::blame_gutter::strip_bidi_controls(&detail.summary),
                MAX_COMMIT_DETAIL_SUMMARY_CHARS,
            ),
            body: crate::blame_gutter::truncate_display(
                &crate::blame_gutter::strip_bidi_controls(&detail.body),
                MAX_COMMIT_DETAIL_BODY_CHARS,
            ),
            author: crate::blame_gutter::truncate_display(
                &crate::blame_gutter::strip_bidi_controls(&detail.author),
                MAX_COMMIT_DETAIL_NAME_CHARS,
            ),
            email: crate::blame_gutter::truncate_display(
                &crate::blame_gutter::strip_bidi_controls(&detail.email),
                MAX_COMMIT_DETAIL_NAME_CHARS,
            ),
            ..detail
        })
    }
}

/// Same three values as `ide-ui`'s own `GitPanel::commit_detail` wrapper
/// -- consistency, not derivation from anything crate-specific.
const MAX_COMMIT_DETAIL_SUMMARY_CHARS: usize = 200;
const MAX_COMMIT_DETAIL_BODY_CHARS: usize = 4000;
const MAX_COMMIT_DETAIL_NAME_CHARS: usize = 200;

fn non_empty(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Parses `YYYY-MM-DD`, rejecting anything but exactly 4/2/2 ASCII digits
/// per field -- an unbounded-width year string here was a real DoS finding
/// (integer overflow) against the pre-fix `ide-ui` version; this fixed-
/// width validation is what closed it, ported verbatim.
fn parse_date_bound(text: &str, end_of_day: bool) -> Result<Option<i64>, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let invalid = || format!("invalid date {trimmed:?}, expected YYYY-MM-DD");
    let mut parts = trimmed.split('-');
    let (Some(y), Some(m), Some(d), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(invalid());
    };
    let is_ascii_digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if y.len() != 4 || m.len() != 2 || d.len() != 2 {
        return Err(invalid());
    }
    if !is_ascii_digits(y) || !is_ascii_digits(m) || !is_ascii_digits(d) {
        return Err(invalid());
    }
    let year: i64 = y.parse().map_err(|_| invalid())?;
    let month: u32 = m.parse().map_err(|_| invalid())?;
    let day: u32 = d.parse().map_err(|_| invalid())?;
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return Err(invalid());
    }
    let seconds_at_midnight = days_from_civil(year, month, day) * 86_400;
    Ok(Some(if end_of_day {
        seconds_at_midnight + 86_399
    } else {
        seconds_at_midnight
    }))
}

fn days_in_month(year: i64, month: u32) -> u32 {
    const DAYS: [u32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if month == 2 && (year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)) {
        29
    } else {
        DAYS[(month - 1) as usize]
    }
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Assigns each commit in `graph` (newest-first, as returned by
/// `commit_graph`) a display lane so parallel branches render in separate
/// columns without overlapping. `ide-tui`'s renderer only uses the lane
/// index for indentation (`docs/features/tui-git-panel.md` §1) -- it
/// draws no connector lines the way `ide-ui`'s egui rendering does. A
/// lane is "reserved" for whichever commit ID is expected to continue it
/// (initially a commit's first parent); a commit reuses its reserved lane
/// if it has one, otherwise takes the first free lane (or opens a new
/// one). Additional parents of a merge commit each reserve their own lane
/// so the branch they came from keeps rendering in its own column until
/// it converges.
pub fn assign_lanes(graph: &[CommitNode]) -> HashMap<String, usize> {
    let mut lanes: Vec<Option<String>> = Vec::new();
    let mut assigned = HashMap::with_capacity(graph.len());

    for commit in graph {
        let lane = lanes
            .iter()
            .position(|slot| slot.as_deref() == Some(commit.id.as_str()))
            .unwrap_or_else(|| match lanes.iter().position(|slot| slot.is_none()) {
                Some(l) => l,
                None => {
                    lanes.push(None);
                    lanes.len() - 1
                }
            });
        assigned.insert(commit.id.clone(), lane);

        lanes[lane] = commit.parents.first().cloned();

        for parent in commit.parents.iter().skip(1) {
            if lanes
                .iter()
                .any(|slot| slot.as_deref() == Some(parent.as_str()))
            {
                continue;
            }
            match lanes.iter().position(|slot| slot.is_none()) {
                Some(l) => lanes[l] = Some(parent.clone()),
                None => lanes.push(Some(parent.clone())),
            }
        }
    }

    assigned
}

#[cfg(test)]
mod tests {
    use super::*;
    use ide_core::DiffLine;
    use std::process::Command;

    fn run(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
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

    /// `-b main` and a repo-local identity make these repos independent of
    /// the runner's ambient git config -- see `crates/tui/src/app.rs`'s
    /// `init_git_repo` doc comment for why both are needed (not just an
    /// `init.defaultBranch` gap: `GitRepo::commit`'s git2 signature never
    /// reads `run`'s `GIT_AUTHOR_NAME` env vars, only actual git config).
    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), &["init", "-q", "-b", "main"]);
        run(dir.path(), &["config", "user.name", "Test"]);
        run(dir.path(), &["config", "user.email", "test@example.com"]);
        dir
    }

    fn commit(dir: &Path, name: &str, content: &str, message: &str) -> String {
        std::fs::write(dir.join(name), content).unwrap();
        run(dir, &["add", "."]);
        run(dir, &["commit", "-q", "-m", message]);
        String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(dir)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string()
    }

    #[test]
    fn refresh_on_non_repo_clears_state_without_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        assert!(!panel.is_repo());
        assert!(panel.graph.is_empty());
        assert!(panel.conflicts.is_empty());
    }

    #[test]
    fn refresh_on_repo_populates_graph() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "one\n", "first");
        let second = commit(dir.path(), "f.txt", "two\n", "second");

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());

        assert!(panel.is_repo());
        assert_eq!(panel.graph.len(), 2);
        assert_eq!(panel.graph[0].id, second);
        assert!(panel.selected_commit.is_none());
    }

    #[test]
    fn refresh_populates_current_branch_and_clears_it_on_non_repo() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "one\n", "first");

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        assert!(panel.current_branch.is_some());

        let non_repo = tempfile::tempdir().unwrap();
        panel.refresh(non_repo.path());
        assert!(panel.current_branch.is_none());
    }

    #[test]
    fn select_commit_loads_diff() {
        let dir = init_repo();
        let first = commit(dir.path(), "f.txt", "x\ny\n", "first");

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.select_commit(&first);

        assert_eq!(panel.selected_commit, Some(first));
        let diffs = panel.diff.expect("diff loaded");
        assert_eq!(diffs.len(), 1);
    }

    #[test]
    fn show_working_tree_diff_for_path_inside_repo() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\nb\n", "init");
        std::fs::write(dir.path().join("f.txt"), "a\nB\n").unwrap();

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.show_working_tree_diff(&dir.path().join("f.txt"));

        let diffs = panel.diff.expect("diff loaded");
        assert_eq!(diffs.len(), 1);
        let lines: Vec<&DiffLine> = diffs[0].hunks.iter().flat_map(|h| &h.lines).collect();
        assert!(lines
            .iter()
            .any(|l| matches!(l, DiffLine::Removed(text, _) if text == "b")));
        assert!(lines
            .iter()
            .any(|l| matches!(l, DiffLine::Added(text, _) if text == "B")));
    }

    #[test]
    fn show_working_tree_diff_for_path_outside_repo_clears_diff() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\n", "init");
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("other.txt");
        std::fs::write(&outside_file, "x").unwrap();

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.select_commit(&commit(dir.path(), "f.txt", "b\n", "second"));
        panel.show_working_tree_diff(&outside_file);

        assert!(panel.diff.is_none());
    }

    fn setup_conflict(dir: &Path) -> PathBuf {
        commit(dir, "f.txt", "base\n", "base");
        run(dir, &["checkout", "-qb", "theirs"]);
        commit(dir, "f.txt", "theirs\n", "theirs change");
        run(dir, &["checkout", "-q", "-"]);
        commit(dir, "f.txt", "ours\n", "ours change");
        let status = Command::new("git")
            .args(["merge", "-q", "theirs"])
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(!status.success(), "expected a merge conflict");
        dir.join("f.txt")
    }

    #[test]
    fn select_conflict_loads_sides_preseeded_with_ours() {
        let dir = init_repo();
        setup_conflict(dir.path());

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        assert_eq!(panel.conflicts, vec![PathBuf::from("f.txt")]);

        panel.select_conflict(Path::new("f.txt"));
        let state = panel.active_conflict.as_ref().expect("conflict loaded");
        assert_eq!(state.result, "ours\n");
        assert!(panel.binary_conflict.is_none());
    }

    #[test]
    fn accept_ours_and_accept_theirs_overwrite_result() {
        let dir = init_repo();
        setup_conflict(dir.path());

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.select_conflict(Path::new("f.txt"));

        panel.accept_theirs();
        assert_eq!(panel.active_conflict.as_ref().unwrap().result, "theirs\n");

        panel.accept_ours();
        assert_eq!(panel.active_conflict.as_ref().unwrap().result, "ours\n");
    }

    #[test]
    fn cancel_conflict_clears_both_active_and_binary() {
        let dir = init_repo();
        setup_conflict(dir.path());

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.select_conflict(Path::new("f.txt"));
        assert!(panel.active_conflict.is_some());

        panel.cancel_conflict();
        assert!(panel.active_conflict.is_none());
        assert!(panel.binary_conflict.is_none());
    }

    #[test]
    fn mark_resolved_stages_and_refreshes_conflicts() {
        let dir = init_repo();
        setup_conflict(dir.path());

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.select_conflict(Path::new("f.txt"));
        panel.active_conflict.as_mut().unwrap().result = "resolved\n".to_string();

        assert_eq!(panel.mark_resolved(), Ok(()));
        assert!(panel.conflicts.is_empty());
        assert!(panel.active_conflict.is_none());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "resolved\n"
        );
    }

    #[test]
    fn mark_resolved_with_no_active_conflict_is_a_noop() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\n", "init");
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        assert_eq!(panel.mark_resolved(), Ok(()));
    }

    #[test]
    fn select_conflict_on_non_utf8_side_sets_binary_conflict() {
        let dir = init_repo();
        commit(dir.path(), "f.bin", "base\n", "base");
        run(dir.path(), &["checkout", "-qb", "theirs"]);
        std::fs::write(dir.path().join("f.bin"), [0xFFu8, 0xFE, 0xFD]).unwrap();
        run(dir.path(), &["add", "."]);
        run(dir.path(), &["commit", "-q", "-m", "theirs change"]);
        run(dir.path(), &["checkout", "-q", "-"]);
        std::fs::write(dir.path().join("f.bin"), "ours\n").unwrap();
        run(dir.path(), &["add", "."]);
        run(dir.path(), &["commit", "-q", "-m", "ours change"]);
        let status = Command::new("git")
            .args(["merge", "-q", "theirs"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(!status.success());

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.select_conflict(Path::new("f.bin"));

        assert!(panel.active_conflict.is_none());
        assert_eq!(panel.binary_conflict, Some(PathBuf::from("f.bin")));
    }

    // ---- T28: staging/commit ----

    #[test]
    fn stage_and_unstage_move_a_path_between_lists() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "one\n", "first");
        std::fs::write(dir.path().join("f.txt"), "two\n").unwrap();

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        assert_eq!(panel.status.unstaged.len(), 1);
        assert!(panel.status.staged.is_empty());

        panel.stage(Path::new("f.txt")).unwrap();
        assert!(panel.status.unstaged.is_empty());
        assert_eq!(panel.status.staged.len(), 1);

        panel.unstage(Path::new("f.txt")).unwrap();
        assert_eq!(panel.status.unstaged.len(), 1);
        assert!(panel.status.staged.is_empty());
    }

    #[test]
    fn request_discard_then_confirm_removes_the_working_tree_change() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "one\n", "first");
        std::fs::write(dir.path().join("f.txt"), "two\n").unwrap();

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.request_discard(Path::new("f.txt"));
        assert!(panel.pending_discard.is_some());

        panel.confirm_discard().unwrap();
        assert!(panel.pending_discard.is_none());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "one\n"
        );
    }

    #[test]
    fn cancel_discard_clears_pending_without_touching_the_file() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "one\n", "first");
        std::fs::write(dir.path().join("f.txt"), "two\n").unwrap();

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.request_discard(Path::new("f.txt"));
        panel.cancel_discard();

        assert!(panel.pending_discard.is_none());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "two\n"
        );
    }

    #[test]
    fn commit_with_empty_message_and_no_amend_is_a_noop() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "one\n", "first");

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        let before = panel.graph.len();

        panel.commit().unwrap();
        assert_eq!(panel.graph.len(), before);
    }

    #[test]
    fn commit_stages_message_creates_a_commit_and_clears_state() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "one\n", "first");
        std::fs::write(dir.path().join("f.txt"), "two\n").unwrap();

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.stage(Path::new("f.txt")).unwrap();
        panel.commit_message = "second".to_string();

        panel.commit().unwrap();

        assert!(panel.commit_message.is_empty());
        assert!(!panel.amend);
        assert_eq!(panel.graph.len(), 2);
        assert_eq!(panel.graph[0].summary, "second");
    }

    // ---- T28: log filter ----

    #[test]
    fn non_empty_trims_and_maps_blank_to_none() {
        assert_eq!(non_empty("  main  "), Some("main".to_string()));
        assert_eq!(non_empty("   "), None);
        assert_eq!(non_empty(""), None);
    }

    #[test]
    fn parse_date_bound_accepts_a_well_formed_date() {
        let start = parse_date_bound("2024-01-02", false).unwrap();
        let end = parse_date_bound("2024-01-02", true).unwrap();
        assert_eq!(end.unwrap() - start.unwrap(), 86_399);
    }

    #[test]
    fn parse_date_bound_blank_is_none() {
        assert_eq!(parse_date_bound("", false).unwrap(), None);
        assert_eq!(parse_date_bound("   ", true).unwrap(), None);
    }

    #[test]
    fn parse_date_bound_rejects_malformed_and_out_of_range_input() {
        assert!(parse_date_bound("2024-1-2", false).is_err());
        assert!(parse_date_bound("2024/01/02", false).is_err());
        assert!(parse_date_bound("2024-13-01", false).is_err());
        assert!(parse_date_bound("2024-02-30", false).is_err());
        assert!(parse_date_bound("not-a-date", false).is_err());
    }

    #[test]
    fn parse_date_bound_rejects_an_unbounded_width_year_without_overflowing() {
        // Regression test for the DoS finding this fixed-width validation
        // closed against the pre-fix `ide-ui` version (integer overflow
        // via an unbounded-width year string).
        let huge_year = "9".repeat(400);
        let text = format!("{huge_year}-01-01");
        assert!(parse_date_bound(&text, false).is_err());
    }

    #[test]
    fn days_in_month_handles_leap_years() {
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2023, 2), 28);
        assert_eq!(days_in_month(1900, 2), 28);
        assert_eq!(days_in_month(2000, 2), 29);
    }

    #[test]
    fn days_from_civil_matches_a_known_epoch_offset() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1970, 1, 2), 1);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
    }

    #[test]
    fn apply_log_filter_by_author_narrows_the_graph() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "one\n", "first");
        run(
            dir.path(),
            &["commit", "--allow-empty", "-q", "-m", "second"],
        );

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        assert_eq!(panel.graph.len(), 2);

        panel.log_filter.query = "second".to_string();
        panel.apply_log_filter();

        assert!(panel.log_filter.error.is_none());
        assert_eq!(panel.graph.len(), 1);
        assert_eq!(panel.graph[0].summary, "second");
    }

    #[test]
    fn apply_log_filter_with_an_unparsable_date_sets_error_and_leaves_graph() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "one\n", "first");

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        let before = panel.graph.len();

        panel.log_filter.since = "not-a-date".to_string();
        panel.apply_log_filter();

        assert!(panel.log_filter.error.is_some());
        assert_eq!(panel.graph.len(), before);
    }

    #[test]
    fn clear_log_filter_resets_state_and_reloads_the_unfiltered_graph() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "one\n", "first");
        run(
            dir.path(),
            &["commit", "--allow-empty", "-q", "-m", "second"],
        );

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.log_filter.query = "second".to_string();
        panel.apply_log_filter();
        assert_eq!(panel.graph.len(), 1);

        panel.clear_log_filter();

        assert!(panel.log_filter.query.is_empty());
        assert_eq!(panel.graph.len(), 2);
    }

    #[test]
    fn show_file_history_then_back_to_log_restores_the_commit_graph() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "one\n", "first");
        commit(dir.path(), "other.txt", "x\n", "second");

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        let unfiltered_len = panel.graph.len();

        panel.show_file_history(Path::new("f.txt"));
        assert_eq!(
            panel.log_filter.viewing_file_history,
            Some(PathBuf::from("f.txt"))
        );
        assert_eq!(panel.graph.len(), 1);

        panel.back_to_log();
        assert!(panel.log_filter.viewing_file_history.is_none());
        assert_eq!(panel.graph.len(), unfiltered_len);
    }

    // ---- T28: branches ----

    #[test]
    fn open_branches_popup_loads_branches_and_reset_is_lazy_on_refresh() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "one\n", "first");
        run(dir.path(), &["branch", "feature"]);

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        assert!(panel.branches.is_empty(), "branches stay lazily loaded");

        panel.open_branches_popup(dir.path());
        assert!(panel.branches_popup.open);
        assert_eq!(panel.branches.len(), 2);

        panel.refresh(dir.path());
        assert_eq!(
            panel.branches.len(),
            2,
            "refresh() must not clear an already-loaded branches list"
        );
    }

    #[test]
    fn close_branches_popup_resets_transient_popup_state() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "one\n", "first");

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.open_branches_popup(dir.path());
        panel.branches_popup.filter = "leftover".to_string();
        panel.branches_popup.selected = 3;

        panel.close_branches_popup();

        assert!(!panel.branches_popup.open);
        assert!(panel.branches_popup.filter.is_empty());
        assert_eq!(panel.branches_popup.selected, 0);
    }

    #[test]
    fn checkout_branch_switches_and_closes_the_popup() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "one\n", "first");
        run(dir.path(), &["branch", "feature"]);

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.open_branches_popup(dir.path());

        panel.checkout_branch(dir.path(), "feature").unwrap();

        assert_eq!(panel.current_branch.as_deref(), Some("feature"));
        assert!(!panel.branches_popup.open);
    }

    #[test]
    fn create_branch_with_checkout_switches_to_the_new_branch() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "one\n", "first");

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.open_branches_popup(dir.path());
        panel.branches_popup.show_new_branch_input = true;
        panel.branches_popup.new_branch_name = "feature".to_string();

        panel.create_branch(dir.path(), "feature", true).unwrap();

        assert_eq!(panel.current_branch.as_deref(), Some("feature"));
        assert!(!panel.branches_popup.open);
        assert!(!panel.branches_popup.show_new_branch_input);
        assert!(panel.branches.iter().any(|b| b.name == "feature"));
    }

    #[test]
    fn request_and_cancel_delete_branch_leaves_the_branch_intact() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "one\n", "first");
        run(dir.path(), &["branch", "feature"]);

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.open_branches_popup(dir.path());

        panel.request_delete_branch("feature");
        assert_eq!(
            panel.branches_popup.pending_delete.as_deref(),
            Some("feature")
        );

        panel.cancel_delete_branch();
        assert!(panel.branches_popup.pending_delete.is_none());
        assert!(panel.branches.iter().any(|b| b.name == "feature"));
    }

    #[test]
    fn confirm_delete_branch_removes_a_fully_merged_branch() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "one\n", "first");
        run(dir.path(), &["branch", "feature"]);

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.open_branches_popup(dir.path());
        panel.request_delete_branch("feature");

        panel.confirm_delete_branch(dir.path(), false).unwrap();

        assert!(panel.branches_popup.pending_delete.is_none());
        assert!(!panel.branches.iter().any(|b| b.name == "feature"));
    }

    #[test]
    fn confirm_delete_branch_on_unmerged_branch_leaves_pending_for_a_force_retry() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "one\n", "first");
        run(dir.path(), &["checkout", "-qb", "feature"]);
        commit(dir.path(), "f.txt", "two\n", "second");
        run(dir.path(), &["checkout", "-q", "main"]);

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.open_branches_popup(dir.path());
        panel.request_delete_branch("feature");

        let err = panel.confirm_delete_branch(dir.path(), false);
        assert!(err.is_err());
        assert_eq!(
            panel.branches_popup.pending_delete.as_deref(),
            Some("feature")
        );

        panel.confirm_delete_branch(dir.path(), true).unwrap();
        assert!(panel.branches_popup.pending_delete.is_none());
        assert!(!panel.branches.iter().any(|b| b.name == "feature"));
    }

    #[test]
    fn merge_branch_up_to_date_closes_the_popup() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "one\n", "first");
        run(dir.path(), &["branch", "feature"]);

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.open_branches_popup(dir.path());

        panel.merge_branch(dir.path(), "feature").unwrap();

        assert!(!panel.merging);
        assert!(!panel.branches_popup.open);
    }

    #[test]
    fn merge_branch_with_conflicts_sets_merging_and_leaves_the_popup_open() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "one\n", "first");
        run(dir.path(), &["checkout", "-qb", "feature"]);
        std::fs::write(dir.path().join("f.txt"), "feature-side\n").unwrap();
        run(dir.path(), &["add", "."]);
        run(dir.path(), &["commit", "-q", "-m", "feature change"]);
        run(dir.path(), &["checkout", "-q", "main"]);
        std::fs::write(dir.path().join("f.txt"), "main-side\n").unwrap();
        run(dir.path(), &["add", "."]);
        run(dir.path(), &["commit", "-q", "-m", "main change"]);

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.open_branches_popup(dir.path());

        panel.merge_branch(dir.path(), "feature").unwrap();

        assert!(panel.merging);
        assert!(
            panel.branches_popup.open,
            "merge_branch itself leaves the popup open on Conflicts -- the \
             caller (App::handle_git_branches_key) closes it and redirects"
        );
        assert!(!panel.conflicts.is_empty());
    }

    // ---- worktrees ----

    #[test]
    fn open_worktrees_popup_loads_worktrees_and_reset_is_lazy_on_refresh() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "one\n", "first");
        let wt_dir = tempfile::tempdir().unwrap();
        let wt_path = wt_dir.path().join("feature");
        run(
            dir.path(),
            &[
                "worktree",
                "add",
                "-b",
                "feature",
                wt_path.to_str().unwrap(),
            ],
        );

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        assert!(
            panel.worktrees_popup.worktrees.is_empty(),
            "worktrees stay lazily loaded"
        );

        panel.open_worktrees_popup(dir.path());
        assert!(panel.worktrees_popup.open);
        assert_eq!(panel.worktrees_popup.worktrees.len(), 1);

        panel.refresh(dir.path());
        assert_eq!(
            panel.worktrees_popup.worktrees.len(),
            1,
            "refresh() must not clear an already-loaded worktrees list"
        );
    }

    #[test]
    fn close_worktrees_popup_resets_transient_popup_state() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "one\n", "first");

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.open_worktrees_popup(dir.path());
        panel.worktrees_popup.new_name = "leftover".to_string();
        panel.worktrees_popup.selected = 3;
        panel.worktrees_popup.adding = true;

        panel.close_worktrees_popup();

        assert!(!panel.worktrees_popup.open);
        assert!(panel.worktrees_popup.new_name.is_empty());
        assert_eq!(panel.worktrees_popup.selected, 0);
        assert!(!panel.worktrees_popup.adding);
    }

    #[test]
    fn create_worktree_with_empty_branch_field_creates_a_new_branch() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "one\n", "first");
        let wt_dir = tempfile::tempdir().unwrap();
        let wt_path = wt_dir.path().join("feature");

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.open_worktrees_popup(dir.path());
        panel.worktrees_popup.adding = true;
        panel.worktrees_popup.new_name = "feature".to_string();
        panel.worktrees_popup.new_path = wt_path.to_str().unwrap().to_string();

        panel.create_worktree();

        assert!(panel.worktrees_popup.error.is_none());
        assert!(!panel.worktrees_popup.adding);
        assert!(panel.worktrees_popup.new_name.is_empty());
        assert!(panel.worktrees_popup.new_path.is_empty());
        assert_eq!(panel.worktrees_popup.worktrees.len(), 1);
        assert_eq!(panel.worktrees_popup.worktrees[0].name, "feature");
        assert!(wt_path.exists());
    }

    #[test]
    fn create_worktree_failure_sets_error_and_keeps_form_fields() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "one\n", "first");

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.open_worktrees_popup(dir.path());
        panel.worktrees_popup.adding = true;
        panel.worktrees_popup.new_name = "bad/name".to_string();
        panel.worktrees_popup.new_path = "/tmp/wherever".to_string();

        panel.create_worktree();

        assert!(panel.worktrees_popup.error.is_some());
        assert!(panel.worktrees_popup.adding, "form stays open on failure");
        assert_eq!(panel.worktrees_popup.new_name, "bad/name");
        assert!(panel.worktrees_popup.worktrees.is_empty());
    }

    #[test]
    fn remove_worktree_deletes_a_clean_worktree() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "one\n", "first");
        let wt_dir = tempfile::tempdir().unwrap();
        let wt_path = wt_dir.path().join("feature");
        run(
            dir.path(),
            &[
                "worktree",
                "add",
                "-b",
                "feature",
                wt_path.to_str().unwrap(),
            ],
        );

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.open_worktrees_popup(dir.path());

        panel.remove_worktree("feature", false);

        assert!(panel.worktrees_popup.error.is_none());
        assert!(panel.worktrees_popup.pending_force_remove.is_none());
        assert!(panel.worktrees_popup.worktrees.is_empty());
        assert!(!wt_path.exists());
    }

    #[test]
    fn remove_worktree_with_uncommitted_changes_sets_pending_force_remove() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "one\n", "first");
        let wt_dir = tempfile::tempdir().unwrap();
        let wt_path = wt_dir.path().join("feature");
        run(
            dir.path(),
            &[
                "worktree",
                "add",
                "-b",
                "feature",
                wt_path.to_str().unwrap(),
            ],
        );
        std::fs::write(wt_path.join("untracked.txt"), "dirty\n").unwrap();

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.open_worktrees_popup(dir.path());

        panel.remove_worktree("feature", false);

        assert!(panel.worktrees_popup.error.is_none());
        assert_eq!(
            panel.worktrees_popup.pending_force_remove.as_deref(),
            Some("feature")
        );
        assert_eq!(panel.worktrees_popup.worktrees.len(), 1);

        panel.remove_worktree("feature", true);

        assert!(panel.worktrees_popup.pending_force_remove.is_none());
        assert!(panel.worktrees_popup.worktrees.is_empty());
    }

    #[test]
    fn worktree_add_field_next_and_prev_cycle_and_wrap() {
        assert_eq!(WorktreeAddField::Name.next(), WorktreeAddField::Path);
        assert_eq!(WorktreeAddField::Path.next(), WorktreeAddField::Branch);
        assert_eq!(WorktreeAddField::Branch.next(), WorktreeAddField::Name);
        assert_eq!(WorktreeAddField::Name.prev(), WorktreeAddField::Branch);
        assert_eq!(WorktreeAddField::Path.prev(), WorktreeAddField::Name);
        assert_eq!(WorktreeAddField::Branch.prev(), WorktreeAddField::Path);
    }

    // ---- assign_lanes ----

    fn node(id: &str, parents: &[&str]) -> CommitNode {
        CommitNode {
            id: id.to_string(),
            short_id: id.to_string(),
            summary: id.to_string(),
            author: "test".to_string(),
            timestamp: 0,
            parents: parents.iter().map(|p| p.to_string()).collect(),
        }
    }

    #[test]
    fn assign_lanes_linear_history_stays_on_one_lane() {
        let graph = vec![node("c", &["b"]), node("b", &["a"]), node("a", &[])];
        let lanes = assign_lanes(&graph);
        assert_eq!(lanes["a"], 0);
        assert_eq!(lanes["b"], 0);
        assert_eq!(lanes["c"], 0);
    }

    #[test]
    fn assign_lanes_diverging_branches_get_separate_lanes_then_converge() {
        let graph = vec![
            node("c", &["b1", "b2"]),
            node("b2", &["a"]),
            node("b1", &["a"]),
            node("a", &[]),
        ];
        let lanes = assign_lanes(&graph);
        assert_eq!(lanes["c"], 0);
        assert_ne!(lanes["b1"], lanes["b2"]);
        assert!(lanes.contains_key("a"));
    }

    #[test]
    fn blame_for_attributes_each_line() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "one\ntwo\n", "first");
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());

        let lines = panel.blame_for(&dir.path().join("f.txt"));

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].summary, "first");
    }

    #[test]
    fn blame_for_an_untracked_path_is_empty() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\n", "init");
        std::fs::write(dir.path().join("untracked.txt"), "x\n").unwrap();
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());

        assert!(panel
            .blame_for(&dir.path().join("untracked.txt"))
            .is_empty());
    }

    #[test]
    fn blame_for_with_no_repo_open_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "a\n").unwrap();
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());

        assert!(panel.blame_for(&file).is_empty());
    }

    #[test]
    fn gutter_marks_for_a_modified_line_reflects_the_working_tree() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\nb\nc\n", "init");
        std::fs::write(dir.path().join("f.txt"), "a\nB\nc\n").unwrap();

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        let marks = panel.gutter_marks_for(&dir.path().join("f.txt"));

        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].line, 1);
        assert_eq!(marks[0].kind, crate::git_gutter::GutterMarkKind::Modified);
    }

    #[test]
    fn gutter_marks_for_an_unchanged_file_is_empty() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\nb\nc\n", "init");

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        assert!(panel.gutter_marks_for(&dir.path().join("f.txt")).is_empty());
    }

    #[test]
    fn gutter_marks_for_with_no_repo_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "a\n").unwrap();

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        assert!(panel.gutter_marks_for(&file).is_empty());
    }

    #[test]
    fn hunks_for_matches_gutter_marks_fors_own_source() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\nb\nc\n", "init");
        std::fs::write(dir.path().join("f.txt"), "a\nB\nc\n").unwrap();

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        let hunks = panel.hunks_for(&dir.path().join("f.txt"));

        assert_eq!(hunks.len(), 1);
        assert_eq!(crate::git_gutter::marks_from_hunks(&hunks).len(), 1);
    }

    #[test]
    fn commit_detail_returns_summary_and_body() {
        let dir = init_repo();
        let id = commit(dir.path(), "f.txt", "a\n", "Summary\n\nBody.");
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());

        let detail = panel.commit_detail(&id).unwrap();

        assert_eq!(detail.summary, "Summary");
        assert_eq!(detail.body, "Body.");
    }

    #[test]
    fn commit_detail_with_no_repo_open_errors() {
        let panel = GitPanel::default();
        assert!(panel.commit_detail("HEAD").is_err());
    }

    #[test]
    fn commit_detail_strips_bidi_controls_from_author_and_email() {
        let dir = init_repo();
        std::fs::write(dir.path().join("f.txt"), "a\n").unwrap();
        run(dir.path(), &["add", "."]);
        let evil_author = "trusted-dev\u{202E} .exe.gnp.suoicilam";
        let status = Command::new("git")
            .args(["commit", "-q", "-m", "looks normal"])
            .current_dir(dir.path())
            .env("GIT_AUTHOR_NAME", evil_author)
            .env("GIT_AUTHOR_EMAIL", "a\u{202E}b@example.com")
            .env("GIT_COMMITTER_NAME", evil_author)
            .env("GIT_COMMITTER_EMAIL", "a\u{202E}b@example.com")
            .status()
            .unwrap();
        assert!(status.success());
        let id = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(dir.path())
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());

        let detail = panel.commit_detail(&id).unwrap();

        assert!(!detail.author.contains('\u{202E}'));
        assert!(!detail.email.contains('\u{202E}'));
    }

    #[test]
    fn commit_detail_caps_an_unbounded_summary_and_body() {
        let dir = init_repo();
        std::fs::write(dir.path().join("f.txt"), "a\n").unwrap();
        run(dir.path(), &["add", "."]);
        let huge_summary = "S".repeat(10_000);
        let huge_body = "B".repeat(50_000);
        let message = format!("{huge_summary}\n\n{huge_body}");
        run(dir.path(), &["commit", "-q", "-m", &message]);
        let id = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(dir.path())
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());

        let detail = panel.commit_detail(&id).unwrap();

        assert!(detail.summary.chars().count() <= MAX_COMMIT_DETAIL_SUMMARY_CHARS);
        assert!(detail.body.chars().count() <= MAX_COMMIT_DETAIL_BODY_CHARS);
    }
}
