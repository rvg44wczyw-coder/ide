//! Source Control panel state/logic: commit graph, side-by-side diff, and
//! three-way conflict resolution, all backed by `ide_core::GitRepo`. See
//! `docs/features/tui-git-panel.md` §2.1/§3 (itself porting `docs/
//! features/git-support.md` §2.2/§3). Rendering lives in `ui.rs`
//! alongside the rest of `App`'s rendering; everything here is plain
//! state transitions, unit-testable without a terminal harness -- ported
//! near-verbatim from `crates/ui/src/git_panel.rs`, which has zero
//! `egui`/`eframe` dependency itself.

use ide_core::{CommitLogFilter, CommitNode, ConflictSides, FileDiff, GitRepo};
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
                self.repo = Some(repo);
            }
            Err(_) => {
                self.repo = None;
                self.graph.clear();
                self.conflicts.clear();
                self.current_branch = None;
            }
        }
        self.selected_commit = None;
        self.diff = None;
        self.active_conflict = None;
        self.binary_conflict = None;
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

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), &["init", "-q"]);
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
}
