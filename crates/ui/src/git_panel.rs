//! Source Control panel state/logic: commit graph, side-by-side diff, and
//! three-way conflict resolution, all backed by `ide_core::GitRepo`. See
//! `docs/features/git-support.md` §2.2/§3. Rendering lives in
//! `app::render` alongside the rest of `IdeApp`'s rendering (same split as
//! the editor/Claude panel); everything here is plain state transitions,
//! unit-testable without a GUI harness.

use crate::editor::blame_gutter::{strip_bidi_controls, truncate_display};
use crate::editor::{marks_from_hunks, GutterMark};
use ide_core::{
    BlameLine, BranchInfo, CommitDetail, CommitLogFilter, CommitNode, ConflictSides, DiffHunk,
    FileDiff, GitError, GitRepo, MergeOutcome, WorkingTreeStatus, WorktreeInfo,
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
/// git-branches-and-blame.md` §2.2.1) — separate from `GitPanel`'s
/// always-loaded `branches` list, the same split `active_conflict`/
/// `conflicts` already keep.
#[derive(Default)]
pub struct BranchesPopupState {
    pub open: bool,
    pub filter: String,
    pub selected: usize,
    pub new_branch_name: String,
    pub show_new_branch_input: bool,
    /// Branch name pending a "not fully merged — force delete?" confirm
    /// (`delete_branch`'s `Err(BranchNotMerged)` lands here instead of
    /// just being shown as an error) — cleared on a successful delete, but
    /// left set on failure so the same inline confirm can offer Force
    /// Delete next without a second click chain.
    pub pending_delete: Option<String>,
}

/// The worktrees popup's own transient UI state (`docs/features/
/// git-worktrees.md` §2.2.1) — separate from `GitPanel`'s always-empty-
/// until-opened `worktrees_popup.worktrees` list, the same split
/// `branches`/`branches_popup` already keep for branches.
#[derive(Default)]
pub struct WorktreesPopupState {
    pub open: bool,
    pub worktrees: Vec<WorktreeInfo>,
    pub new_name: String,
    pub new_path: String,
    /// Empty means "create a new branch named `new_name`" (§2.1's
    /// `branch: None` case) — the UI surfaces this as placeholder text,
    /// not a separate checkbox, since it's exactly one field either way.
    pub new_branch: String,
    pub error: Option<String>,
    /// Set when a plain (non-force) `remove_worktree` call fails with
    /// `WorktreeHasUncommittedChanges` or a locked worktree, so the popup
    /// can offer a force-confirm button instead of just showing the error
    /// — same two-step pattern `E2`'s branch-delete `BranchNotMerged`
    /// confirm already uses.
    pub pending_force_remove: Option<String>,
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
    /// a selected path (non-UTF-8/binary side, §3's last bullet) — the UI
    /// shows a placeholder message for this specific path and offers no
    /// Resolve UI, rather than the three-way panel.
    pub binary_conflict: Option<PathBuf>,
    /// Cached at `refresh()` time, same pattern as `graph`/`conflicts`.
    pub current_branch: Option<String>,
    pub status: WorkingTreeStatus,
    pub commit_message: String,
    pub amend: bool,
    /// A path awaiting a user's confirm/cancel on Discard -- the Commit
    /// panel's own small modal, distinct from the editor's unrelated
    /// "discard unsaved tab changes" modal (`IdeApp::pending_confirm`).
    pub pending_discard: Option<PathBuf>,
    /// Loaded by `open_branches_popup` and refreshed after every mutating
    /// branch operation — not eagerly loaded by `refresh()` itself, since a
    /// full `refresh()` also runs on a manual Refresh click and eagerly
    /// re-listing branches on every one of those would be needless I/O the
    /// popup being closed doesn't benefit from (`git-branches-and-blame.md`
    /// §2.2.1).
    pub branches: Vec<BranchInfo>,
    pub branches_popup: BranchesPopupState,
    /// `true` between a `merge_branch` call that returned `Conflicts(_)`
    /// and the resulting commit actually landing — purely a UI label/
    /// default-message concern (§2.2.1): while `true`, the existing commit
    /// UI shows "Commit Merge" instead of "Commit". Cleared the moment
    /// `commit()` succeeds, and reset to `false` by `refresh()` alongside
    /// `active_conflict`/`pending_discard` (a manual Refresh or a project
    /// switch mid-merge loses only this cosmetic label, never the
    /// underlying `MERGE_HEAD` state `commit()` itself still detects).
    pub merging: bool,
    /// Loaded by `open_worktrees_popup` and refreshed after every mutating
    /// worktree operation — same lazy-load convention `branches`/
    /// `branches_popup` already keep (`git-worktrees.md` §2.2.1).
    pub worktrees_popup: WorktreesPopupState,
    /// The Log tab's own filter-bar/file-history state (`docs/features/
    /// git-log-viewer.md` §2.2).
    pub log_filter: LogFilterState,
}

/// The log viewer's own filter-bar state — kept separate from
/// `GitPanel::graph` itself (which stays "last query's result set",
/// mirroring how `worktrees_popup.worktrees`/`branches` are already
/// separate from their popups' input fields).
#[derive(Default)]
pub struct LogFilterState {
    pub branch: String,
    pub author: String,
    pub path: String,
    /// Free-typed date bounds (`YYYY-MM-DD`); parsed to Unix-seconds
    /// bounds only when applying the filter (§3.2) — kept as raw text
    /// here so a partially-typed date doesn't get silently discarded
    /// mid-edit.
    pub since: String,
    pub until: String,
    pub query: String,
    /// Set when applying produced an error (an unresolvable `branch`, or
    /// an unparsable `since`/`until`) — shown inline, same pattern
    /// `worktrees_popup.error`/`branches_popup`'s inline error already
    /// use. The graph is left as whatever it last successfully was, not
    /// cleared, on a failed apply.
    pub error: Option<String>,
    /// `true` while `GitPanel::graph` holds a `file_history` result
    /// instead of a `commit_graph` result — the filter bar is hidden
    /// while this is set (§3.3: file history doesn't compose with the
    /// other filters, so showing controls that don't apply would be
    /// misleading) and a "← Back to Log" affordance takes its place.
    pub viewing_file_history: Option<PathBuf>,
}

impl GitPanel {
    pub fn is_repo(&self) -> bool {
        self.repo.is_some()
    }

    /// (Re-)opens the repository at `project_root` and reloads the graph
    /// and conflicts list. Not being a repo is not an error — it just
    /// clears all git state (doc §3's "not a repository" message). Called
    /// on project open/create and on manual Refresh, so a `git init` run
    /// outside the app is picked up without restarting.
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
        // A different repository (project switch) or a plain manual
        // Refresh both make a stale filter/file-history view actively
        // misleading -- e.g. an `author` substring typed against the
        // previous repo's contributors, or `viewing_file_history` pointing
        // at a path whose history no longer applies to the graph this
        // call just loaded unfiltered.
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
    /// editor tab's `Buffer::path()`) into `diff`, converting to a
    /// repo-relative path by stripping `workdir()` as a prefix — never
    /// any other path construction (doc §3, §4's path-provenance rule). A
    /// path outside the repo, or no diff (unchanged/untracked/binary),
    /// clears `diff` rather than erroring. Canonicalizes `absolute_path`
    /// first (`Project::root()`/`GitRepo::workdir()` are always
    /// canonical, so an uncanonicalized input would otherwise fail the
    /// prefix strip on any platform where the path involves a symlink,
    /// e.g. macOS's `/tmp` -> `/private/tmp`).
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

    /// `absolute_path`'s working-tree diff against `HEAD`, independent of
    /// whatever `self.diff` currently holds (a commit's diff, or nothing)
    /// -- the editor gutter's own consumer, refreshed every frame
    /// regardless of which view is showing (`docs/features/
    /// editor-git-gutter.md` §2.3). Empty with no repo, an untracked file,
    /// or a path outside the repo.
    fn hunks_for_impl(&self, absolute_path: &Path) -> Vec<DiffHunk> {
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

    pub fn hunks_for(&self, absolute_path: &Path) -> Vec<DiffHunk> {
        self.hunks_for_impl(absolute_path)
    }

    pub fn gutter_marks_for(&self, absolute_path: &Path) -> Vec<GutterMark> {
        marks_from_hunks(&self.hunks_for_impl(absolute_path))
    }

    /// Selects a conflicted path — must come from `conflicts` (never any
    /// other source, doc §3/§4's path-provenance rule) — and loads its
    /// sides. Sets `binary_conflict` instead of `active_conflict` if
    /// `conflict_sides()` errors (non-UTF-8/binary side).
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
    /// that side is `None` — a delete). No-op with no active conflict.
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

    /// Calls `resolve_conflict(path, result)` for the active conflict,
    /// re-queries `conflicts()` on success (doc §3), and clears the
    /// active-conflict panel. On failure, leaves `active_conflict`
    /// untouched (so the user's edit isn't lost) and returns the error for
    /// the caller to surface. No-op returning `Ok(())` if there's no
    /// active conflict or no open repository.
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

    /// Refreshes `status` from the open repository. No-op with no repo.
    /// A transient git error mid-frame degrades to "nothing changed", not
    /// an error banner spamming every frame -- same permissive-on-error
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
    /// fresh click, not a modal that lingers on failure.
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
    /// layer's own empty-message rejection, checked here too so the UI
    /// can simply grey out the button rather than surface a round-tripped
    /// `GitError`).
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
        // `graph` above is unconditionally a plain unfiltered log now --
        // if the user had been viewing a file's history, leaving that flag
        // set would hide the filter bar and show "Back to Log" over data
        // that already *is* the log.
        self.log_filter.viewing_file_history = None;
        Ok(())
    }

    /// Rebuilds a `CommitLogFilter` from `log_filter`'s text fields and
    /// reloads `graph` via the two-argument `commit_graph`. On any parse/
    /// git error, sets `log_filter.error` and leaves `graph` untouched
    /// (§3.2 of `git-log-viewer.md`) -- never a half-applied filter
    /// silently showing the wrong graph.
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
    /// graph -- the toolbar's "Clear Filter" action.
    pub fn clear_log_filter(&mut self) {
        self.log_filter = LogFilterState::default();
        self.apply_log_filter();
    }

    /// Loads `path`'s rename-aware history into `graph` via
    /// `GitRepo::file_history` and sets `log_filter.viewing_file_history`.
    /// `path` must already be repository-relative -- the caller
    /// (`CommandAction::ShowFileHistory`'s handler in `app.rs`) strips the
    /// project root off the active tab's absolute path first, the same
    /// convention `show_working_tree_diff`'s own `diff_file` call follows.
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
    /// `LogFilterState` currently holds (not necessarily unfiltered -- if
    /// the user had an active filter before switching to file history,
    /// returning restores it rather than discarding it).
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
    /// transient state (`git-branches-and-blame.md` §2.2.1/§2.2.2).
    /// Defensively re-opens the repository if it somehow isn't loaded yet
    /// (mirrors `refresh`'s own not-a-repo handling) rather than showing an
    /// empty popup with no way to recover short of a manual Refresh.
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

    /// Checks out `name` (safe-mode checkout, `switch_branch`'s own doc
    /// comment) and closes the popup on success -- on error, the popup
    /// stays open so the caller can surface the git2 error text inline
    /// (§3's "surface the real error" convention).
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

    /// Creates `name` from `HEAD` and, if `checkout` is set, switches to it
    /// -- one button for "create" and "create and checkout" (§2.1's
    /// `create_branch` doc comment: the core layer never bakes that policy
    /// in). The branch list and popup input are refreshed/cleared even if
    /// the follow-up checkout fails (the branch itself was still created),
    /// but the popup only closes when the whole operation succeeds.
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

    /// Marks `name` as pending a delete confirm (§2.2.2's inline confirm,
    /// mirroring `request_discard`). Never called for the branch `HEAD`
    /// currently points at -- the popup must not offer Delete on that row
    /// at all (§3).
    pub fn request_delete_branch(&mut self, name: &str) {
        self.branches_popup.pending_delete = Some(name.to_string());
    }

    pub fn cancel_delete_branch(&mut self) {
        self.branches_popup.pending_delete = None;
    }

    /// Attempts to delete the pending branch. On `BranchNotMerged` (or any
    /// other error), `pending_delete` is left set so the same inline
    /// confirm can retry with `force: true` without a second click chain --
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

    /// Starts a merge of `name` into the current branch (§2.1's
    /// `merge_branch` doc comment lists the four outcomes). On
    /// `Conflicts`, refreshes state (which repopulates `conflicts` from
    /// the repo's own index -- already reflecting the merge -- the same
    /// single-source-of-truth `refresh()` always uses) and sets `merging`
    /// plus a pre-filled default commit message, but leaves the popup
    /// open. On `Merged`/`FastForward`/`UpToDate`, refreshes and closes
    /// the popup.
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

    /// Opens the worktrees popup with a freshly loaded list
    /// (`git-worktrees.md` §2.2.1). Defensively re-opens the repository if
    /// it somehow isn't loaded yet, same as `open_branches_popup` — a
    /// project being open doesn't guarantee `self.repo` is `Some`:
    /// `is_command_enabled` only gates this command on a project being
    /// open, not on it being a git repository, so a project that becomes a
    /// git repo mid-session (an external `git init`) would otherwise leave
    /// this popup stuck showing an empty list with no way to recover short
    /// of a manual Refresh.
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
    /// on success or `worktrees_popup.error` on failure (§2.2.1). Not
    /// eagerly called by `refresh()` itself — same lazy-load reasoning
    /// `reload_branches` already documents for `branches`.
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
    /// `new_branch` fields (§2.2.1): an empty `new_branch` becomes `None`
    /// (core's own "create a new branch named `name`" default). On
    /// success, clears the form and refreshes the list; on failure, sets
    /// `error` and leaves the form fields as-is so the user can fix and
    /// retry rather than retype. Doesn't close the popup either way —
    /// unlike `create_branch`, adding a worktree isn't a context switch.
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
                self.worktrees_popup.error = None;
                self.refresh_worktrees();
            }
            Err(e) => self.worktrees_popup.error = Some(e.to_string()),
        }
    }

    /// Removes `name` (§2.2.1). On a `WorktreeHasUncommittedChanges` or
    /// `WorktreeLocked` failure with `force: false`, sets
    /// `pending_force_remove` *instead of* `error` — the popup's confirm
    /// step uses a fixed message ("has uncommitted changes / is locked --
    /// remove anyway?", §2.2.2) rather than the raw error text, the same
    /// two-step pattern `request_delete_branch`/`confirm_delete_branch`
    /// already use for `BranchNotMerged`. Any other failure (including a
    /// retry with `force: true` that still fails) surfaces as `error`.
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

    /// `absolute_path`'s per-line blame against `HEAD`, converted to a
    /// repo-relative path the same way `hunks_for`/`show_working_tree_diff`
    /// already do (canonicalize, then strip the repo's workdir prefix) --
    /// empty with no repo, an untracked path, or a path outside the repo
    /// (`GitRepo::blame_file`'s own "not an error" treatment, §2.1).
    pub fn blame_for(&self, absolute_path: &Path) -> Vec<BlameLine> {
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

    /// Full detail for one commit, for the blame popup (§2.2.3) --
    /// `author`/`email`/`summary`/`body` are sanitized (bidi controls
    /// stripped) and length-capped here, the one place `CommitDetail`
    /// leaves `GitRepo` on its way to a UI label: git imposes no length
    /// limit on any of these fields, and an untrusted (e.g. cloned)
    /// repository's commit metadata must never carry an unterminated
    /// bidi override or an unbounded string into `egui`'s text layout
    /// (`docs/security-findings/git-branches-and-blame-ui-2026-09-01.md`,
    /// findings 1-2).
    pub fn commit_detail(&self, commit_id: &str) -> Result<CommitDetail, String> {
        let Some(repo) = &self.repo else {
            return Err("no repository open".to_string());
        };
        let detail = repo.commit_detail(commit_id).map_err(|e| e.to_string())?;
        Ok(CommitDetail {
            summary: truncate_display(
                &strip_bidi_controls(&detail.summary),
                MAX_COMMIT_DETAIL_SUMMARY_CHARS,
            ),
            body: truncate_display(
                &strip_bidi_controls(&detail.body),
                MAX_COMMIT_DETAIL_BODY_CHARS,
            ),
            author: truncate_display(
                &strip_bidi_controls(&detail.author),
                MAX_COMMIT_DETAIL_NAME_CHARS,
            ),
            email: truncate_display(
                &strip_bidi_controls(&detail.email),
                MAX_COMMIT_DETAIL_NAME_CHARS,
            ),
            ..detail
        })
    }
}

/// Display caps for `GitPanel::commit_detail`'s sanitized fields -- git
/// itself imposes none of these (`docs/security-findings/
/// git-branches-and-blame-ui-2026-09-01.md`, finding 2).
const MAX_COMMIT_DETAIL_SUMMARY_CHARS: usize = 200;
const MAX_COMMIT_DETAIL_BODY_CHARS: usize = 4000;
const MAX_COMMIT_DETAIL_NAME_CHARS: usize = 200;

/// Assigns each commit in `graph` (newest-first, as returned by
/// `commit_graph`) a display lane so parallel branches render in separate
/// columns without overlapping — a rendering-layer concern; `CommitNode`
/// itself only carries parent OIDs (doc §3). A lane is "reserved" for
/// whichever commit ID is expected to continue it (initially a commit's
/// first parent); a commit reuses its reserved lane if it has one,
/// otherwise takes the first free lane (or opens a new one). Additional
/// parents of a merge commit each reserve their own lane so the branch
/// they came from keeps rendering in its own column until it converges.
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

/// Trims `text` and turns an empty result into `None` -- the shared rule
/// every `LogFilterState` text field uses to decide whether it's "set" for
/// `CommitLogFilter` purposes (§3.2 of `git-log-viewer.md`).
fn non_empty(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Parses a `YYYY-MM-DD` bound into Unix seconds -- the start of that day
/// if `end_of_day` is `false`, or its last second (`23:59:59`) if `true`,
/// so a user-typed `until` date covers the whole day inclusively. Empty
/// input is `Ok(None)` (no bound), matching `LogFilterState`'s "free text
/// until applied" contract.
///
/// `ide-ui` has no timezone-database dependency (`CLAUDE.md`'s Dependencies
/// table lists none, and this feature adds none per its own §6) and Rust's
/// standard library exposes no local-timezone offset without one, so
/// unlike the doc's "local timezone" wording this treats the parsed date
/// as UTC. Documented here and in the feature doc's revision notes as a
/// deliberate scope call, not an oversight.
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

/// Days in `month` (`1..=12`) of proleptic-Gregorian `year`, leap years
/// included -- `parse_date_bound`'s own validation, so a typo like
/// `2026-02-30` is rejected as an error rather than silently rolling over
/// into March (`days_from_civil` doesn't validate this itself; it's
/// well-defined, just not the calendar date the user typed).
fn days_in_month(year: i64, month: u32) -> u32 {
    const DAYS: [u32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if month == 2 && (year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)) {
        29
    } else {
        DAYS[(month - 1) as usize]
    }
}

/// Howard Hinnant's `days_from_civil` (proleptic Gregorian, valid for any
/// year) -- days since the Unix epoch (1970-01-01) for civil date
/// `(y, m, d)`, `1 <= m <= 12` (caller-validated). See
/// <https://howardhinnant.github.io/date_algorithms.html#days_from_civil>.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m as i64 + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use ide_core::{DiffLine, DiffSpan};
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

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), &["init", "-q"]);
        // Local, not just `run`'s per-invocation `GIT_AUTHOR_*`/
        // `GIT_COMMITTER_*` env vars: those only affect commits made
        // through the `git` CLI here (`commit()` below), but `GitPanel::
        // commit` (the production path several tests exercise) goes
        // through `ide_core::GitRepo::commit`, which resolves its
        // signature via `git2::Repository::signature()` -- pure config-
        // file lookup, unaffected by those env vars. A CI runner with no
        // global git identity set hits `config value 'user.name' was not
        // found` there without this (v0.1.1 tag, run 33502675537).
        run(dir.path(), &["config", "user.name", "Test User"]);
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
    fn refresh_picks_up_git_init_run_outside_the_app() {
        let dir = tempfile::tempdir().unwrap();
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        assert!(!panel.is_repo());

        run(dir.path(), &["init", "-q"]);
        panel.refresh(dir.path());
        assert!(panel.is_repo());
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

    /// Like `commit`, but lets the caller pick author identity and/or an
    /// explicit commit date (`GIT_AUTHOR_DATE`/`GIT_COMMITTER_DATE`, ISO
    /// 8601 accepted directly by git) instead of `run`'s fixed
    /// `"Test User"`/`"test@example.com"` -- needed to exercise
    /// `CommitLogFilter`'s `author`/`since`/`until` fields against commits
    /// that actually differ on those axes.
    fn commit_as(
        dir: &Path,
        name: &str,
        content: &str,
        message: &str,
        author_name: &str,
        author_email: &str,
        date: Option<&str>,
    ) -> String {
        std::fs::write(dir.join(name), content).unwrap();
        run(dir, &["add", "."]);
        let mut cmd = Command::new("git");
        cmd.args(["commit", "-q", "-m", message])
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", author_name)
            .env("GIT_AUTHOR_EMAIL", author_email)
            .env("GIT_COMMITTER_NAME", author_name)
            .env("GIT_COMMITTER_EMAIL", author_email);
        if let Some(date) = date {
            cmd.env("GIT_AUTHOR_DATE", date)
                .env("GIT_COMMITTER_DATE", date);
        }
        assert!(cmd.status().unwrap().success());
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
    fn apply_log_filter_by_author_narrows_graph() {
        let dir = init_repo();
        commit_as(
            dir.path(),
            "a.txt",
            "a\n",
            "by alice",
            "Alice",
            "alice@example.com",
            None,
        );
        let bob = commit_as(
            dir.path(),
            "b.txt",
            "b\n",
            "by bob",
            "Bob",
            "bob@example.com",
            None,
        );

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        assert_eq!(panel.graph.len(), 2);

        panel.log_filter.author = "bob".to_string();
        panel.apply_log_filter();

        assert_eq!(panel.log_filter.error, None);
        assert_eq!(panel.graph.len(), 1);
        assert_eq!(panel.graph[0].id, bob);
    }

    #[test]
    fn apply_log_filter_by_author_matches_email_too() {
        let dir = init_repo();
        commit_as(
            dir.path(),
            "a.txt",
            "a\n",
            "by someone",
            "Someone Else",
            "carol@example.com",
            None,
        );

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.log_filter.author = "CAROL".to_string();
        panel.apply_log_filter();

        assert_eq!(panel.graph.len(), 1);
    }

    #[test]
    fn apply_log_filter_by_query_narrows_graph() {
        let dir = init_repo();
        commit(dir.path(), "a.txt", "a\n", "add feature");
        let fix = commit(dir.path(), "b.txt", "b\n", "fix panic on empty input");

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.log_filter.query = "fix panic".to_string();
        panel.apply_log_filter();

        assert_eq!(panel.graph.len(), 1);
        assert_eq!(panel.graph[0].id, fix);
    }

    #[test]
    fn apply_log_filter_by_path_narrows_graph() {
        let dir = init_repo();
        commit(dir.path(), "a.txt", "a\n", "touch a");
        let b = commit(dir.path(), "b.txt", "b\n", "touch b");

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.log_filter.path = "b.txt".to_string();
        panel.apply_log_filter();

        assert_eq!(panel.graph.len(), 1);
        assert_eq!(panel.graph[0].id, b);
    }

    #[test]
    fn apply_log_filter_by_since_and_until_narrows_graph() {
        let dir = init_repo();
        commit_as(
            dir.path(),
            "a.txt",
            "a\n",
            "old",
            "Test",
            "test@example.com",
            Some("2020-01-01T00:00:00+0000"),
        );
        let mid = commit_as(
            dir.path(),
            "b.txt",
            "b\n",
            "mid",
            "Test",
            "test@example.com",
            Some("2022-06-15T00:00:00+0000"),
        );
        commit_as(
            dir.path(),
            "c.txt",
            "c\n",
            "new",
            "Test",
            "test@example.com",
            Some("2024-01-01T00:00:00+0000"),
        );

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.log_filter.since = "2021-01-01".to_string();
        panel.log_filter.until = "2023-01-01".to_string();
        panel.apply_log_filter();

        assert_eq!(panel.log_filter.error, None);
        assert_eq!(panel.graph.len(), 1);
        assert_eq!(panel.graph[0].id, mid);
    }

    #[test]
    fn apply_log_filter_by_branch_walks_from_that_branch() {
        let dir = init_repo();
        commit(dir.path(), "a.txt", "a\n", "base");
        run(dir.path(), &["checkout", "-q", "-b", "topic"]);
        let topic_commit = commit(dir.path(), "b.txt", "b\n", "on topic");
        run(dir.path(), &["checkout", "-q", "-"]);

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        assert_eq!(panel.graph.len(), 1);

        panel.log_filter.branch = "topic".to_string();
        panel.apply_log_filter();

        assert_eq!(panel.log_filter.error, None);
        assert_eq!(panel.graph.len(), 2);
        assert_eq!(panel.graph[0].id, topic_commit);
    }

    #[test]
    fn apply_log_filter_with_unresolvable_branch_sets_error_and_leaves_graph() {
        let dir = init_repo();
        commit(dir.path(), "a.txt", "a\n", "base");

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        let before = panel.graph.clone();

        panel.log_filter.branch = "does-not-exist".to_string();
        panel.apply_log_filter();

        assert!(panel.log_filter.error.is_some());
        assert_eq!(panel.graph, before);
    }

    #[test]
    fn apply_log_filter_with_unparsable_date_sets_error_and_leaves_graph() {
        let dir = init_repo();
        commit(dir.path(), "a.txt", "a\n", "base");

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        let before = panel.graph.clone();

        panel.log_filter.since = "not-a-date".to_string();
        panel.apply_log_filter();

        assert!(panel.log_filter.error.is_some());
        assert_eq!(panel.graph, before);
    }

    #[test]
    fn clear_log_filter_resets_state_and_reloads_full_graph() {
        let dir = init_repo();
        commit(dir.path(), "a.txt", "a\n", "add feature");
        commit(dir.path(), "b.txt", "b\n", "fix panic");

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.log_filter.query = "fix panic".to_string();
        panel.apply_log_filter();
        assert_eq!(panel.graph.len(), 1);

        panel.clear_log_filter();

        assert_eq!(panel.log_filter.query, "");
        assert_eq!(panel.log_filter.error, None);
        assert_eq!(panel.graph.len(), 2);
    }

    #[test]
    fn show_file_history_sets_flag_and_loads_history_then_back_to_log_restores_view() {
        let dir = init_repo();
        commit(dir.path(), "tracked.txt", "one\n", "first");
        commit(dir.path(), "other.txt", "x\n", "unrelated");
        let second = commit(dir.path(), "tracked.txt", "two\n", "second");

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        assert_eq!(panel.graph.len(), 3);

        panel.show_file_history(Path::new("tracked.txt"));

        assert_eq!(
            panel.log_filter.viewing_file_history,
            Some(PathBuf::from("tracked.txt"))
        );
        assert_eq!(panel.graph.len(), 2);
        assert_eq!(panel.graph[0].id, second);

        panel.back_to_log();

        assert_eq!(panel.log_filter.viewing_file_history, None);
        assert_eq!(panel.graph.len(), 3);
    }

    #[test]
    fn back_to_log_restores_an_active_filter_rather_than_clearing_it() {
        let dir = init_repo();
        commit(dir.path(), "tracked.txt", "one\n", "add feature");
        let fix = commit(dir.path(), "other.txt", "x\n", "fix panic");

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.log_filter.query = "fix panic".to_string();
        panel.apply_log_filter();
        assert_eq!(panel.graph.len(), 1);

        panel.show_file_history(Path::new("tracked.txt"));
        assert_eq!(panel.graph.len(), 1);

        panel.back_to_log();

        assert_eq!(panel.log_filter.viewing_file_history, None);
        assert_eq!(panel.log_filter.query, "fix panic");
        assert_eq!(panel.graph.len(), 1);
        assert_eq!(panel.graph[0].id, fix);
    }

    #[test]
    fn refresh_resets_log_filter_state() {
        let dir = init_repo();
        commit(dir.path(), "a.txt", "a\n", "first");

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.log_filter.author = "someone".to_string();
        panel.log_filter.error = Some("stale".to_string());
        panel.log_filter.viewing_file_history = Some(PathBuf::from("a.txt"));

        panel.refresh(dir.path());

        assert_eq!(panel.log_filter.author, "");
        assert_eq!(panel.log_filter.error, None);
        assert_eq!(panel.log_filter.viewing_file_history, None);
    }

    #[test]
    fn commit_clears_viewing_file_history_but_keeps_filter_text() {
        let dir = init_repo();
        commit(dir.path(), "tracked.txt", "one\n", "first");

        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.show_file_history(Path::new("tracked.txt"));
        panel.log_filter.author = "someone".to_string();

        std::fs::write(dir.path().join("tracked.txt"), "two\n").unwrap();
        run(dir.path(), &["add", "."]);
        panel.commit_message = "second".to_string();
        panel.commit().unwrap();

        assert_eq!(panel.log_filter.viewing_file_history, None);
        assert_eq!(panel.log_filter.author, "someone");
        assert_eq!(panel.graph.len(), 2);
    }

    #[test]
    fn non_empty_trims_and_maps_blank_to_none() {
        assert_eq!(non_empty("  hello  "), Some("hello".to_string()));
        assert_eq!(non_empty(""), None);
        assert_eq!(non_empty("   "), None);
    }

    #[test]
    fn parse_date_bound_empty_is_no_bound() {
        assert_eq!(parse_date_bound("", false).unwrap(), None);
        assert_eq!(parse_date_bound("   ", true).unwrap(), None);
    }

    #[test]
    fn parse_date_bound_matches_the_docs_worked_example() {
        // `git-log-viewer.md` §5: 2026-01-01T00:00:00Z == 1_767_225_600.
        assert_eq!(
            parse_date_bound("2026-01-01", false).unwrap(),
            Some(1_767_225_600)
        );
    }

    #[test]
    fn parse_date_bound_end_of_day_is_last_second() {
        assert_eq!(
            parse_date_bound("2026-01-01", true).unwrap(),
            Some(1_767_225_600 + 86_399)
        );
    }

    #[test]
    fn parse_date_bound_rejects_malformed_input() {
        assert!(parse_date_bound("2026/01/01", false).is_err());
        assert!(parse_date_bound("2026-13-01", false).is_err());
        assert!(parse_date_bound("2026-01-32", false).is_err());
        assert!(parse_date_bound("not-a-date", false).is_err());
        assert!(parse_date_bound("2026-01", false).is_err());
    }

    #[test]
    fn parse_date_bound_rejects_a_day_that_does_not_exist_in_that_month() {
        // 2026 is not a leap year -- Feb has 28 days.
        assert!(parse_date_bound("2026-02-30", false).is_err());
        assert!(parse_date_bound("2026-02-29", false).is_err());
        assert!(parse_date_bound("2026-04-31", false).is_err());
        assert!(parse_date_bound("2026-01-00", false).is_err());
    }

    #[test]
    fn parse_date_bound_accepts_feb_29_on_a_leap_year() {
        assert!(parse_date_bound("2024-02-29", false).is_ok());
    }

    #[test]
    fn days_in_month_handles_leap_year_rules() {
        assert_eq!(days_in_month(2024, 2), 29); // divisible by 4
        assert_eq!(days_in_month(2026, 2), 28); // not divisible by 4
        assert_eq!(days_in_month(1900, 2), 28); // divisible by 100, not 400
        assert_eq!(days_in_month(2000, 2), 29); // divisible by 400
        assert_eq!(days_in_month(2026, 4), 30);
        assert_eq!(days_in_month(2026, 1), 31);
    }

    #[test]
    fn days_from_civil_matches_known_epoch_offsets() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
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
        // "b" -> "B": no shared prefix/suffix between the two single-char
        // lines, so each side's whole text is one intraline span (matches
        // ide-core's git::tests::diff_file_single_line_modification_pairs_spans).
        assert!(lines.contains(&&DiffLine::Removed(
            "b".to_string(),
            vec![DiffSpan { start: 0, end: 1 }]
        )));
        assert!(lines.contains(&&DiffLine::Added(
            "B".to_string(),
            vec![DiffSpan { start: 0, end: 1 }]
        )));
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
        assert_eq!(marks[0].kind, crate::editor::GutterMarkKind::Modified);
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
        assert_eq!(crate::editor::marks_from_hunks(&hunks).len(), 1);
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

    // ---- status/stage/unstage/discard/commit ----

    #[test]
    fn refresh_populates_status_and_resets_commit_state() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\n", "init");
        std::fs::write(dir.path().join("f.txt"), "b\n").unwrap();

        let mut panel = GitPanel {
            commit_message: "leftover".to_string(),
            amend: true,
            pending_discard: Some(PathBuf::from("f.txt")),
            ..GitPanel::default()
        };
        panel.refresh(dir.path());

        assert_eq!(panel.status.unstaged.len(), 1);
        assert!(panel.status.staged.is_empty());
        assert!(panel.commit_message.is_empty());
        assert!(!panel.amend);
        assert!(panel.pending_discard.is_none());
    }

    #[test]
    fn refresh_on_non_repo_clears_status() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\n", "init");
        std::fs::write(dir.path().join("f.txt"), "b\n").unwrap();
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        assert!(!panel.status.unstaged.is_empty());

        let non_repo = tempfile::tempdir().unwrap();
        panel.refresh(non_repo.path());
        assert_eq!(panel.status, WorkingTreeStatus::default());
    }

    #[test]
    fn stage_moves_an_entry_from_unstaged_to_staged() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\n", "init");
        std::fs::write(dir.path().join("f.txt"), "b\n").unwrap();
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());

        assert_eq!(panel.stage(Path::new("f.txt")), Ok(()));

        assert!(panel.status.unstaged.is_empty());
        assert_eq!(panel.status.staged[0].path, PathBuf::from("f.txt"));
    }

    #[test]
    fn stage_with_no_repo_open_is_a_noop() {
        let mut panel = GitPanel::default();
        assert_eq!(panel.stage(Path::new("f.txt")), Ok(()));
    }

    #[test]
    fn unstage_moves_an_entry_back_to_unstaged() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\n", "init");
        std::fs::write(dir.path().join("f.txt"), "b\n").unwrap();
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.stage(Path::new("f.txt")).unwrap();
        assert!(!panel.status.staged.is_empty());

        assert_eq!(panel.unstage(Path::new("f.txt")), Ok(()));

        assert!(panel.status.staged.is_empty());
        assert_eq!(panel.status.unstaged[0].path, PathBuf::from("f.txt"));
    }

    #[test]
    fn stage_rejects_path_escaping_workdir() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\n", "init");
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());

        assert!(panel.stage(Path::new("../outside.txt")).is_err());
    }

    #[test]
    fn request_discard_then_cancel_discard_clears_pending() {
        let mut panel = GitPanel::default();
        panel.request_discard(Path::new("f.txt"));
        assert_eq!(panel.pending_discard, Some(PathBuf::from("f.txt")));

        panel.cancel_discard();
        assert!(panel.pending_discard.is_none());
    }

    #[test]
    fn confirm_discard_with_no_pending_is_a_noop() {
        let mut panel = GitPanel::default();
        assert_eq!(panel.confirm_discard(), Ok(()));
    }

    #[test]
    fn confirm_discard_reverts_the_file_and_clears_pending() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\n", "init");
        std::fs::write(dir.path().join("f.txt"), "b\n").unwrap();
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.request_discard(Path::new("f.txt"));

        assert_eq!(panel.confirm_discard(), Ok(()));

        assert!(panel.pending_discard.is_none());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "a\n"
        );
        assert!(panel.status.unstaged.is_empty());
    }

    #[test]
    fn confirm_discard_clears_pending_even_on_error() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\n", "init");
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.request_discard(Path::new("../outside.txt"));

        assert!(panel.confirm_discard().is_err());
        assert!(panel.pending_discard.is_none());
    }

    #[test]
    fn commit_creates_a_commit_and_clears_message() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\n", "init");
        std::fs::write(dir.path().join("f.txt"), "b\n").unwrap();
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.stage(Path::new("f.txt")).unwrap();
        panel.commit_message = "second commit".to_string();

        assert_eq!(panel.commit(), Ok(()));

        assert!(panel.commit_message.is_empty());
        assert!(!panel.amend);
        assert!(panel.status.staged.is_empty());
        assert_eq!(panel.graph.len(), 2);
        assert_eq!(panel.graph[0].summary, "second commit");
    }

    #[test]
    fn commit_with_empty_message_and_no_amend_is_a_noop() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\n", "init");
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.commit_message = "   ".to_string();

        assert_eq!(panel.commit(), Ok(()));
        assert_eq!(panel.graph.len(), 1);
    }

    #[test]
    fn commit_with_amend_replaces_the_head_commit_message() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\n", "init");
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.amend = true;
        panel.commit_message = "amended message".to_string();

        assert_eq!(panel.commit(), Ok(()));

        assert!(!panel.amend);
        assert_eq!(panel.graph.len(), 1);
        assert_eq!(panel.graph[0].summary, "amended message");
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
        // c (merge, parents b1,b2) -> b1 -> a, and b2 -> a
        let graph = vec![
            node("c", &["b1", "b2"]),
            node("b2", &["a"]),
            node("b1", &["a"]),
            node("a", &[]),
        ];
        let lanes = assign_lanes(&graph);
        assert_eq!(lanes["c"], 0);
        assert_ne!(lanes["b1"], lanes["b2"]);
        // both branches converge back onto whichever lane reaches `a` first
        assert!(lanes.contains_key("a"));
    }

    // ---- branches popup ----

    #[test]
    fn open_branches_popup_loads_the_list_and_resets_transient_state() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\n", "init");
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.branches_popup.filter = "leftover".to_string();

        panel.open_branches_popup(dir.path());

        assert!(panel.branches_popup.open);
        assert!(panel.branches_popup.filter.is_empty());
        assert_eq!(panel.branches.len(), 1);
        assert!(panel.branches[0].is_head);
    }

    #[test]
    fn close_branches_popup_resets_the_whole_state() {
        let mut panel = GitPanel::default();
        panel.branches_popup.open = true;
        panel.branches_popup.new_branch_name = "x".to_string();
        panel.branches_popup.pending_delete = Some("y".to_string());

        panel.close_branches_popup();

        assert!(!panel.branches_popup.open);
        assert!(panel.branches_popup.new_branch_name.is_empty());
        assert!(panel.branches_popup.pending_delete.is_none());
    }

    #[test]
    fn create_branch_from_head_without_checkout_does_not_switch() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\n", "init");
        let main_name = String::from_utf8(
            Command::new("git")
                .args(["symbolic-ref", "--short", "HEAD"])
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

        assert_eq!(panel.create_branch(dir.path(), "feature", false), Ok(()));

        assert_eq!(panel.current_branch, Some(main_name));
        assert!(panel.branches.iter().any(|b| b.name == "feature"));
    }

    #[test]
    fn create_branch_with_checkout_switches_and_closes_popup() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\n", "init");
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.open_branches_popup(dir.path());
        panel.branches_popup.new_branch_name = "feature".to_string();

        assert_eq!(panel.create_branch(dir.path(), "feature", true), Ok(()));

        assert_eq!(panel.current_branch, Some("feature".to_string()));
        assert!(!panel.branches_popup.open);
        assert!(panel.branches_popup.new_branch_name.is_empty());
    }

    #[test]
    fn create_branch_on_an_existing_name_errors_without_clearing_the_input() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\n", "init");
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.create_branch(dir.path(), "feature", false).unwrap();
        panel.open_branches_popup(dir.path());
        panel.branches_popup.new_branch_name = "feature".to_string();

        assert!(panel.create_branch(dir.path(), "feature", false).is_err());

        assert_eq!(panel.branches_popup.new_branch_name, "feature");
    }

    #[test]
    fn checkout_branch_switches_and_closes_popup() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\n", "init");
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.create_branch(dir.path(), "feature", false).unwrap();
        panel.open_branches_popup(dir.path());

        assert_eq!(panel.checkout_branch(dir.path(), "feature"), Ok(()));

        assert_eq!(panel.current_branch, Some("feature".to_string()));
        assert!(!panel.branches_popup.open);
    }

    #[test]
    fn checkout_branch_with_uncommitted_changes_errors_and_keeps_popup_open() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\n", "init");
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.create_branch(dir.path(), "feature", false).unwrap();
        run(dir.path(), &["checkout", "-q", "feature"]);
        commit(dir.path(), "f.txt", "b\n", "on feature");
        run(dir.path(), &["checkout", "-q", "-"]);
        std::fs::write(dir.path().join("f.txt"), "uncommitted\n").unwrap();
        panel.open_branches_popup(dir.path());

        assert!(panel.checkout_branch(dir.path(), "feature").is_err());

        assert!(panel.branches_popup.open);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "uncommitted\n"
        );
    }

    #[test]
    fn delete_branch_flow_requests_then_confirms() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\n", "init");
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.create_branch(dir.path(), "feature", false).unwrap();
        panel.open_branches_popup(dir.path());

        panel.request_delete_branch("feature");
        assert_eq!(
            panel.branches_popup.pending_delete,
            Some("feature".to_string())
        );

        assert_eq!(panel.confirm_delete_branch(dir.path(), false), Ok(()));

        assert!(panel.branches_popup.pending_delete.is_none());
        assert!(!panel.branches.iter().any(|b| b.name == "feature"));
    }

    #[test]
    fn cancel_delete_branch_clears_pending_without_deleting() {
        let mut panel = GitPanel::default();
        panel.request_delete_branch("feature");

        panel.cancel_delete_branch();

        assert!(panel.branches_popup.pending_delete.is_none());
    }

    #[test]
    fn confirm_delete_branch_with_no_pending_is_a_noop() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\n", "init");
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        assert_eq!(panel.confirm_delete_branch(dir.path(), false), Ok(()));
    }

    #[test]
    fn confirm_delete_branch_on_unmerged_errors_and_keeps_pending_for_a_force_retry() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\n", "init");
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.create_branch(dir.path(), "feature", false).unwrap();
        run(dir.path(), &["checkout", "-q", "feature"]);
        commit(dir.path(), "g.txt", "b\n", "on feature only");
        run(dir.path(), &["checkout", "-q", "-"]);
        panel.open_branches_popup(dir.path());
        panel.request_delete_branch("feature");

        assert!(panel.confirm_delete_branch(dir.path(), false).is_err());
        assert_eq!(
            panel.branches_popup.pending_delete,
            Some("feature".to_string())
        );

        assert_eq!(panel.confirm_delete_branch(dir.path(), true), Ok(()));
        assert!(panel.branches_popup.pending_delete.is_none());
        assert!(!panel.branches.iter().any(|b| b.name == "feature"));
    }

    #[test]
    fn merge_branch_up_to_date_closes_popup() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\n", "init");
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.create_branch(dir.path(), "feature", false).unwrap();
        panel.open_branches_popup(dir.path());

        assert_eq!(panel.merge_branch(dir.path(), "feature"), Ok(()));

        assert!(!panel.branches_popup.open);
        assert!(!panel.merging);
    }

    #[test]
    fn merge_branch_fast_forward_closes_popup_and_updates_graph() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\n", "init");
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.create_branch(dir.path(), "feature", false).unwrap();
        run(dir.path(), &["checkout", "-q", "feature"]);
        commit(dir.path(), "g.txt", "b\n", "on feature");
        run(dir.path(), &["checkout", "-q", "-"]);
        panel.open_branches_popup(dir.path());

        assert_eq!(panel.merge_branch(dir.path(), "feature"), Ok(()));

        assert!(!panel.branches_popup.open);
        assert_eq!(panel.graph.len(), 2);
    }

    #[test]
    fn merge_branch_conflicts_sets_merging_and_prefills_message_without_closing_popup() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\n", "init");
        let main_name = String::from_utf8(
            Command::new("git")
                .args(["symbolic-ref", "--short", "HEAD"])
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
        panel.create_branch(dir.path(), "feature", false).unwrap();
        run(dir.path(), &["checkout", "-q", "feature"]);
        commit(dir.path(), "f.txt", "from feature\n", "feature edits f");
        run(dir.path(), &["checkout", "-q", "-"]);
        commit(dir.path(), "f.txt", "from main\n", "main edits f");
        panel.open_branches_popup(dir.path());

        assert_eq!(panel.merge_branch(dir.path(), "feature"), Ok(()));

        assert!(panel.branches_popup.open);
        assert!(panel.merging);
        assert_eq!(panel.conflicts, vec![PathBuf::from("f.txt")]);
        assert_eq!(
            panel.commit_message,
            format!("Merge branch 'feature' into {main_name}")
        );

        // Finishing it via the existing commit flow clears `merging`.
        panel.select_conflict(Path::new("f.txt"));
        panel.active_conflict.as_mut().unwrap().result = "resolved\n".to_string();
        panel.mark_resolved().unwrap();
        assert_eq!(panel.commit(), Ok(()));
        assert!(!panel.merging);
    }

    #[test]
    fn refresh_resets_merging() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\n", "init");
        let mut panel = GitPanel {
            merging: true,
            ..GitPanel::default()
        };
        panel.refresh(dir.path());
        assert!(!panel.merging);
    }

    // ---- blame / commit_detail ----

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
        assert_eq!(detail.author, "trusted-dev .exe.gnp.suoicilam");
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
        assert!(detail.summary.ends_with('\u{2026}'));
        assert!(detail.body.ends_with('\u{2026}'));
    }

    // ---- worktrees popup ----

    #[test]
    fn open_worktrees_popup_loads_the_list_and_resets_transient_state() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\n", "init");
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.worktrees_popup.error = Some("leftover".to_string());

        panel.open_worktrees_popup(dir.path());

        assert!(panel.worktrees_popup.open);
        assert!(panel.worktrees_popup.error.is_none());
        assert!(panel.worktrees_popup.worktrees.is_empty());
    }

    #[test]
    fn open_worktrees_popup_defensively_reopens_a_repo_initialized_after_first_refresh() {
        // Mirrors `refresh_picks_up_git_init_run_outside_the_app`: this is
        // the code path `is_command_enabled`'s project-only gate can reach
        // with `self.repo` still `None` (rev finding #3, E8 fix round).
        let dir = tempfile::tempdir().unwrap();
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        assert!(!panel.is_repo());

        run(dir.path(), &["init", "-q"]);
        panel.open_worktrees_popup(dir.path());

        assert!(panel.is_repo());
        assert!(panel.worktrees_popup.open);
    }

    #[test]
    fn close_worktrees_popup_resets_the_whole_state() {
        let mut panel = GitPanel::default();
        panel.worktrees_popup.open = true;
        panel.worktrees_popup.new_name = "x".to_string();
        panel.worktrees_popup.error = Some("y".to_string());

        panel.close_worktrees_popup();

        assert!(!panel.worktrees_popup.open);
        assert!(panel.worktrees_popup.new_name.is_empty());
        assert!(panel.worktrees_popup.error.is_none());
    }

    #[test]
    fn refresh_worktrees_on_non_repo_clears_the_list() {
        let mut panel = GitPanel::default();
        panel.worktrees_popup.worktrees = vec![WorktreeInfo {
            name: "stale".to_string(),
            path: PathBuf::from("/stale"),
            branch: None,
            is_locked: false,
        }];

        panel.refresh_worktrees();

        assert!(panel.worktrees_popup.worktrees.is_empty());
    }

    #[test]
    fn refresh_worktrees_populates_from_the_repo() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\n", "init");
        let worktree_dir = tempfile::tempdir().unwrap();
        let wt_path = worktree_dir.path().join("wt");
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel
            .repo
            .as_ref()
            .unwrap()
            .add_worktree("wt", &wt_path, None)
            .unwrap();

        panel.refresh_worktrees();

        assert_eq!(panel.worktrees_popup.worktrees.len(), 1);
        assert_eq!(panel.worktrees_popup.worktrees[0].name, "wt");
    }

    #[test]
    fn create_worktree_adds_it_and_clears_the_form() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\n", "init");
        let worktree_dir = tempfile::tempdir().unwrap();
        let wt_path = worktree_dir.path().join("wt");
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel.worktrees_popup.new_name = "wt".to_string();
        panel.worktrees_popup.new_path = wt_path.to_string_lossy().to_string();

        panel.create_worktree();

        assert!(panel.worktrees_popup.error.is_none());
        assert!(panel.worktrees_popup.new_name.is_empty());
        assert!(panel.worktrees_popup.new_path.is_empty());
        assert_eq!(panel.worktrees_popup.worktrees.len(), 1);
        assert_eq!(panel.worktrees_popup.worktrees[0].name, "wt");
    }

    #[test]
    fn create_worktree_with_a_taken_name_sets_error_and_keeps_the_form() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\n", "init");
        let worktree_dir = tempfile::tempdir().unwrap();
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel
            .repo
            .as_ref()
            .unwrap()
            .add_worktree("wt", worktree_dir.path().join("first"), None)
            .unwrap();
        panel.worktrees_popup.new_name = "wt".to_string();
        panel.worktrees_popup.new_path = worktree_dir
            .path()
            .join("second")
            .to_string_lossy()
            .to_string();

        panel.create_worktree();

        assert!(panel.worktrees_popup.error.is_some());
        // Left as-is so the user can fix and retry rather than retype
        // (create_worktree's own doc comment).
        assert_eq!(panel.worktrees_popup.new_name, "wt");
    }

    #[test]
    fn remove_worktree_removes_it_and_refreshes_the_list() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\n", "init");
        let worktree_dir = tempfile::tempdir().unwrap();
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel
            .repo
            .as_ref()
            .unwrap()
            .add_worktree("wt", worktree_dir.path().join("wt"), None)
            .unwrap();
        panel.refresh_worktrees();
        assert_eq!(panel.worktrees_popup.worktrees.len(), 1);

        panel.remove_worktree("wt", false);

        assert!(panel.worktrees_popup.error.is_none());
        assert!(panel.worktrees_popup.pending_force_remove.is_none());
        assert!(panel.worktrees_popup.worktrees.is_empty());
    }

    #[test]
    fn remove_worktree_with_uncommitted_changes_sets_pending_force_remove_then_force_removes() {
        let dir = init_repo();
        commit(dir.path(), "f.txt", "a\n", "init");
        let worktree_dir = tempfile::tempdir().unwrap();
        let wt_path = worktree_dir.path().join("wt");
        let mut panel = GitPanel::default();
        panel.refresh(dir.path());
        panel
            .repo
            .as_ref()
            .unwrap()
            .add_worktree("wt", &wt_path, None)
            .unwrap();
        std::fs::write(wt_path.join("f.txt"), "dirty\n").unwrap();

        panel.remove_worktree("wt", false);
        assert!(panel.worktrees_popup.error.is_none());
        assert_eq!(
            panel.worktrees_popup.pending_force_remove.as_deref(),
            Some("wt")
        );
        // Still registered -- the plain call above must not have removed it.
        panel.refresh_worktrees();
        assert_eq!(panel.worktrees_popup.worktrees.len(), 1);

        panel.remove_worktree("wt", true);

        assert!(panel.worktrees_popup.error.is_none());
        assert!(panel.worktrees_popup.pending_force_remove.is_none());
        assert!(panel.worktrees_popup.worktrees.is_empty());
    }
}
