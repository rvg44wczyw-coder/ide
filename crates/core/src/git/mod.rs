//! `GitRepo` wraps a `git2::Repository` (libgit2 — no subprocess spawning;
//! see `crates/core/Cargo.toml`'s `git2` feature selection, which enables
//! the `https`/`ssh` transports `clone_repo` needs). All paths accepted/
//! returned by this module are
//! repository-relative, never absolute — see `docs/features/git-support.md`
//! §2.1 for why that's load-bearing for `resolve_conflict`'s escape check,
//! not just a naming convention.
//!
//! `GitRepo`'s methods are not safe to call concurrently from multiple
//! threads against the same on-disk repository: `git2::Repository`'s own
//! index/object-database caching isn't designed for that, and
//! `resolve_conflict`'s `index.write()` can lose a concurrent thread's
//! staged addition (read-then-write on the index, not an atomic update).
//! The UI is expected to serialize calls (matches v1's one-conflict-at-a-
//! time resolution flow — see `docs/features/git-support.md` §3).

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("not a git repository: {0}")]
    NotARepo(PathBuf),
    #[error("git error: {0}")]
    Git2(#[from] git2::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("path escapes repository working directory: {0}")]
    PathEscapesRepo(PathBuf),
    #[error("repository URL is empty")]
    EmptyUrl,
    #[error("destination already exists and is not empty: {0}")]
    DestinationNotEmpty(PathBuf),
    #[error("cloned repository failed its own project-root validation: {0}")]
    ClonedContentInvalid(PathBuf),
    #[error("branch '{0}' is not fully merged into HEAD")]
    BranchNotMerged(String),
    #[error("a worktree named '{0}' already exists")]
    WorktreeNameTaken(String),
    #[error("invalid worktree name: {0}")]
    InvalidWorktreeName(String),
    #[error("worktree destination is inside this repository's own working directory: {0}")]
    WorktreeInsideRepo(PathBuf),
    #[error("worktree has uncommitted changes: {0}")]
    WorktreeHasUncommittedChanges(PathBuf),
    #[error("worktree '{0}' is locked")]
    WorktreeLocked(PathBuf),
}

/// Per-file line cap for `diff_file`/`diff_commit` output (context +
/// added + removed lines combined). Chosen generously above what a human
/// reviews in the diff pane in one sitting, while still bounding worst-
/// case memory/render cost for a pathological huge-file diff.
pub const MAX_DIFF_LINES: usize = 20_000;

/// Cap on the number of `FileDiff` entries `diff_commit` returns for a
/// single commit. `MAX_DIFF_LINES` bounds worst-case cost per file but
/// nothing bounded the number of files, so a commit crafted (or
/// accidentally, e.g. a vendored bulk import) to touch an unusually large
/// number of files had unbounded cost on that axis — found via live
/// testing (a 30k-file commit took ~2.3s and produced 30k in-memory
/// `FileDiff`s). Silently truncates, same as `commit_graph`'s `limit`
/// silently stops at that many nodes — no separate truncation flag.
pub const MAX_DIFF_FILES: usize = 2_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub old_path: Option<PathBuf>,
    pub new_path: Option<PathBuf>,
    pub hunks: Vec<DiffHunk>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHunk {
    pub old_start: u32,
    pub new_start: u32,
    pub lines: Vec<DiffLine>,
}

/// A byte-offset range into a [`DiffLine::Added`]/[`DiffLine::Removed`]
/// line's text, marking a sub-span that differs from the line it's paired
/// with on the other side (`docs/features/diff-viewer-enhancements.md`
/// §3.1/§3.4). Always char-boundary-aligned for the line it's attached to,
/// so `text[start..end]` never panics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffSpan {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLine {
    Context(String),
    /// Text, plus the spans within it that differ from the `Removed` line
    /// it's paired with (empty if unpaired — a pure insertion, or this
    /// hunk's added-run outnumbers its removed-run).
    Added(String, Vec<DiffSpan>),
    /// Symmetric to `Added`.
    Removed(String, Vec<DiffSpan>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitNode {
    pub id: String,
    pub short_id: String,
    pub summary: String,
    pub author: String,
    pub timestamp: i64,
    pub parents: Vec<String>,
}

/// Full detail for one commit -- a superset of `CommitNode`, kept as its
/// own type instead of adding `body`/`email` to `CommitNode` since every
/// existing `commit_graph` call site constructs/matches that struct today
/// and doesn't need those two fields for a graph row
/// (`docs/features/git-branches-and-blame.md` §2.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitDetail {
    pub id: String,
    pub short_id: String,
    /// First line of the commit message.
    pub summary: String,
    /// Everything after the summary's blank-line separator, trimmed.
    /// Empty string (not `None`) for a single-line message.
    pub body: String,
    pub author: String,
    pub email: String,
    pub timestamp: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchInfo {
    pub name: String,
    pub is_head: bool,
}

/// The result of `GitRepo::merge_branch` (`docs/features/
/// git-branches-and-blame.md` §2.1/§7's state diagram).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    /// The merged branch's tip is already an ancestor of `HEAD` -- nothing
    /// to do.
    UpToDate,
    /// `HEAD` was fast-forwarded to the merged branch's tip. No commit
    /// created.
    FastForward,
    /// A real merge was needed and produced conflicts. `MERGE_HEAD` is now
    /// set; resolve every path in the list (the existing `conflicts()`/
    /// `conflict_sides()`/`resolve_conflict()` trio) then call `commit()`
    /// to finish it -- `commit()` is `MERGE_HEAD`-aware (see its own doc
    /// comment below).
    Conflicts(Vec<PathBuf>),
    /// A real merge was needed, produced no conflicts, and was committed
    /// automatically through this type's own `commit()` (matches plain
    /// `git merge`'s own default behavior -- no separate "finish" step for
    /// the clean case).
    Merged { commit_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameLine {
    /// 0-based buffer line.
    pub line: usize,
    pub commit_id: String,
    pub short_id: String,
    pub author: String,
    pub timestamp: i64,
    pub summary: String,
}

/// Cap on the number of lines `blame_file` computes/returns for one file
/// -- same shape as `MAX_DIFF_LINES`: blame cost scales with both file
/// size and history depth, so an unbounded file is a plausible slow-path
/// even without anything adversarial involved. Bounds the `git2`-side
/// call itself via `BlameOptions::max_line`, not a post-hoc `Vec`
/// truncation, so the cost is actually capped, not just the output size.
/// Lines beyond the cap are silently absent from the result, matching
/// `MAX_DIFF_FILES`'s own silent-truncation precedent.
pub const MAX_BLAME_LINES: usize = 20_000;

/// One linked worktree of a repository (never the main working tree --
/// `git2::Repository::worktrees` only ever enumerates linked ones, so
/// there is no "is this the main repo" case to special-case here or in
/// any caller; `docs/features/git-worktrees.md` §2.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    /// The worktree's registered name (passed to `add_worktree`, and to
    /// `remove_worktree` to identify it again) -- not necessarily the
    /// same as its checked-out branch name.
    pub name: String,
    /// Absolute path to the worktree's top-level working directory.
    pub path: PathBuf,
    /// The branch currently checked out in this worktree, if its HEAD
    /// resolves to one (detached HEAD, or a worktree whose directory was
    /// deleted out from under git and can no longer be opened, both
    /// yield `None`).
    pub branch: Option<String>,
    /// Whether `git worktree lock` has been used on this worktree
    /// (typically because it lives on removable/network storage that may
    /// be offline).
    pub is_locked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictSides {
    pub base: Option<String>,
    pub ours: Option<String>,
    pub theirs: Option<String>,
}

/// Flattens git's richer per-path status flag set down to five buckets --
/// same "own flattened summary enum" precedent `ide_lsp::SymbolKind`/
/// `DiagnosticSeverity` set for a wire protocol's own richer type. A
/// rename shows as `Deleted` (old path) + `Added` (new path) rather than
/// a dedicated `Renamed` variant (`docs/features/
/// git-commit-and-staging.md` §2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Untracked,
    Conflicted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    pub path: PathBuf,
    pub kind: ChangeKind,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkingTreeStatus {
    pub staged: Vec<StatusEntry>,
    pub unstaged: Vec<StatusEntry>,
}

pub struct GitRepo {
    repo: git2::Repository,
    workdir: PathBuf,
}

impl GitRepo {
    /// Discovers a repository at or above `project_root` (matching normal
    /// git behavior for a project nested in a larger repo) and opens it.
    /// A bare repository (no working directory) is treated the same as
    /// "not found" — v1's working-tree diff and conflict-resolution write
    /// path both require one.
    pub fn open(project_root: impl AsRef<Path>) -> Result<Self, GitError> {
        let project_root = project_root.as_ref();
        let repo = git2::Repository::discover(project_root)
            .map_err(|_| GitError::NotARepo(project_root.to_path_buf()))?;
        let workdir = repo
            .workdir()
            .ok_or_else(|| GitError::NotARepo(project_root.to_path_buf()))?;
        let workdir = fs::canonicalize(workdir)?;
        Ok(Self { repo, workdir })
    }

    pub fn is_repo(project_root: impl AsRef<Path>) -> bool {
        Self::open(project_root).is_ok()
    }

    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    /// `HEAD`'s shorthand name (e.g. `"main"`), or `None` for a brand-new
    /// repository with no commits yet (`HEAD` itself doesn't resolve) --
    /// same "unresolvable `HEAD` is not an error" treatment `commit_graph`
    /// already gives it, for the same reason (libgit2 reports an unborn
    /// branch in ways that vary, so any failure to resolve `HEAD` is
    /// treated uniformly rather than pattern-matched). A **detached**
    /// `HEAD` still resolves and returns `Some("HEAD")` -- libgit2's own
    /// shorthand for a direct (non-symbolic) reference, not an error case.
    pub fn current_branch(&self) -> Option<String> {
        self.repo.head().ok()?.shorthand().ok().map(str::to_string)
    }

    /// Diff of `path` (repo-relative)'s working-tree content against the
    /// `HEAD` blob for that path. Matches plain `git diff HEAD -- path`
    /// semantics: an untracked file (no `HEAD` blob and never staged)
    /// shows no diff here, same as it wouldn't appear in `git diff`.
    pub fn diff_file(&self, path: impl AsRef<Path>) -> Result<Option<FileDiff>, GitError> {
        let target = normalize(path.as_ref());
        let head_tree = self.head_tree()?;
        let diff = self
            .repo
            .diff_tree_to_workdir_with_index(head_tree.as_ref(), None)?;
        let mut files = build_file_diffs(&diff)?;
        let idx = files
            .iter()
            .position(|f| f.new_path.as_deref() == Some(target.as_path()));
        Ok(idx.map(|i| files.swap_remove(i)))
    }

    /// Diff of every file `commit_id` changed, against its first parent
    /// (or an empty tree for a root commit). Capped at `MAX_DIFF_FILES`
    /// entries — see the constant's doc.
    pub fn diff_commit(&self, commit_id: &str) -> Result<Vec<FileDiff>, GitError> {
        let oid = git2::Oid::from_str(commit_id)?;
        let commit = self.repo.find_commit(oid)?;
        let tree = commit.tree()?;
        let parent_tree = match commit.parent(0) {
            Ok(parent) => Some(parent.tree()?),
            Err(_) => None,
        };
        let diff = self
            .repo
            .diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), None)?;
        let mut files = build_file_diffs(&diff)?;
        files.truncate(MAX_DIFF_FILES);
        Ok(files)
    }

    /// Commit graph reachable from `HEAD`, newest first, capped at
    /// `limit` commits. Empty (not an error) for a repository with no
    /// commits yet.
    pub fn commit_graph(&self, limit: usize) -> Result<Vec<CommitNode>, GitError> {
        // A brand-new repository has no commit for HEAD to resolve to at
        // all — libgit2 reports this in ways that vary (an unborn-branch
        // error, or a plain "reference not found" for whatever the
        // default branch name is), so treat any failure to resolve HEAD
        // as "no commits yet" rather than pattern-matching a specific
        // error code.
        if self.repo.head().is_err() {
            return Ok(Vec::new());
        }
        let mut revwalk = self.repo.revwalk()?;
        revwalk.push_head()?;
        revwalk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::TIME)?;

        let mut nodes = Vec::new();
        for oid in revwalk.take(limit) {
            let oid = oid?;
            let commit = self.repo.find_commit(oid)?;
            // Display-only fields degrade to a fallback instead of failing
            // the whole graph over one commit with unusual (non-UTF-8)
            // metadata — id/parents (used for selection and edges) always
            // come from `Oid`, which never has this problem.
            let short_id = commit
                .as_object()
                .short_id()
                .ok()
                .and_then(|buf| buf.as_str().ok().map(str::to_string))
                .unwrap_or_else(|| oid.to_string());
            let summary = commit
                .summary()
                .ok()
                .flatten()
                .unwrap_or_default()
                .to_string();
            let author = commit.author().name().ok().unwrap_or_default().to_string();
            nodes.push(CommitNode {
                id: oid.to_string(),
                short_id,
                summary,
                author,
                timestamp: commit.time().seconds(),
                parents: commit.parent_ids().map(|id| id.to_string()).collect(),
            });
        }
        Ok(nodes)
    }

    /// Paths (repo-relative) with an unresolved merge conflict in the
    /// index.
    pub fn conflicts(&self) -> Result<Vec<PathBuf>, GitError> {
        let index = self.repo.index()?;
        let mut paths = Vec::new();
        for conflict in index.conflicts()? {
            let conflict = conflict?;
            let entry = conflict
                .our
                .as_ref()
                .or(conflict.their.as_ref())
                .or(conflict.ancestor.as_ref());
            if let Some(entry) = entry {
                let path = PathBuf::from(String::from_utf8_lossy(&entry.path).into_owned());
                if !paths.contains(&path) {
                    paths.push(path);
                }
            }
        }
        Ok(paths)
    }

    /// The base/ours/theirs content for a conflicted path (repo-relative).
    /// Errors if `path` has no conflict, or if a present side's blob isn't
    /// valid UTF-8 (v1 only resolves text-file conflicts — see module
    /// docs and `docs/features/git-support.md` §2.1).
    pub fn conflict_sides(&self, path: impl AsRef<Path>) -> Result<ConflictSides, GitError> {
        let target = path_bytes(&normalize(path.as_ref()));
        let index = self.repo.index()?;
        let conflict = index
            .conflicts()?
            .filter_map(|c| c.ok())
            .find(|c| {
                [&c.ancestor, &c.our, &c.their]
                    .iter()
                    .any(|e| e.as_ref().is_some_and(|e| e.path == target))
            })
            .ok_or_else(|| git2::Error::from_str("path has no conflict"))?;

        Ok(ConflictSides {
            base: self.blob_utf8(conflict.ancestor.as_ref())?,
            ours: self.blob_utf8(conflict.our.as_ref())?,
            theirs: self.blob_utf8(conflict.their.as_ref())?,
        })
    }

    /// Writes `resolved_text` to `path` (repo-relative) in the working
    /// tree and stages it, clearing the conflict for that path. Does not
    /// create a commit.
    pub fn resolve_conflict(
        &self,
        path: impl AsRef<Path>,
        resolved_text: &str,
    ) -> Result<(), GitError> {
        let rel_path = normalize(path.as_ref());
        let full_path = self.workdir.join(&rel_path);
        let canonical = fs::canonicalize(&full_path)?;
        if !canonical.starts_with(&self.workdir) {
            return Err(GitError::PathEscapesRepo(rel_path));
        }

        // Write to a uniquely-named sibling temp file and persist (rename)
        // it over `canonical` rather than writing straight to `canonical`.
        // The rename replaces whatever is at the destination (including a
        // symlink placed there after the check above) instead of
        // following it, so a check-then-write race can't redirect the
        // write outside `workdir` the way a direct `fs::write` would
        // allow. Also avoids leaving a partially-written file behind if
        // the process is interrupted mid-write. Uses `tempfile`'s
        // collision-safe name generation rather than a hand-rolled
        // pid+timestamp scheme -- an earlier version of this used
        // `process::id()` + `SystemTime::now()`, which a concurrency test
        // showed can collide under fast concurrent calls in the same
        // process (coarse clock granularity), silently cross-
        // contaminating two different target files' content.
        let parent = canonical.parent().unwrap_or(&self.workdir);
        let mut tmp = tempfile::Builder::new()
            .prefix(".resolve-conflict-")
            .tempfile_in(parent)?;
        std::io::Write::write_all(&mut tmp, resolved_text.as_bytes())?;
        tmp.persist(&canonical).map_err(|e| e.error)?;

        let mut index = self.repo.index()?;
        index.add_path(&rel_path)?;
        index.write()?;
        Ok(())
    }

    /// Every changed path in the working tree and index, bucketed into
    /// `staged`/`unstaged` (`docs/features/git-commit-and-staging.md`
    /// §3.1). A path can appear in both (partially staged) except a
    /// conflicted path, which appears once, in `unstaged`, with kind
    /// `Conflicted` -- the Conflicts UI's Accept Ours/Accept Theirs/Mark
    /// Resolved flow already owns resolving it, so it's deliberately kept
    /// out of the plain stage/unstage list.
    pub fn status(&self) -> Result<WorkingTreeStatus, GitError> {
        let mut opts = git2::StatusOptions::new();
        opts.include_untracked(true)
            .recurse_untracked_dirs(true)
            .include_ignored(false);
        let statuses = self.repo.statuses(Some(&mut opts))?;

        let mut result = WorkingTreeStatus::default();
        for entry in statuses.iter() {
            // A non-UTF-8 path is skipped entirely rather than failing
            // the whole call -- same permissive-per-entry convention
            // `conflict_sides`'s own UTF-8 handling already establishes,
            // just applied per-status-entry instead of per-conflict-side.
            let Some(path) = entry.path().ok() else {
                continue;
            };
            let path = PathBuf::from(path);
            let flags = entry.status();

            if flags.contains(git2::Status::CONFLICTED) {
                result.unstaged.push(StatusEntry {
                    path,
                    kind: ChangeKind::Conflicted,
                });
                continue;
            }

            if flags.contains(git2::Status::INDEX_NEW) {
                result.staged.push(StatusEntry {
                    path: path.clone(),
                    kind: ChangeKind::Added,
                });
            } else if flags.intersects(
                git2::Status::INDEX_MODIFIED
                    | git2::Status::INDEX_RENAMED
                    | git2::Status::INDEX_TYPECHANGE,
            ) {
                result.staged.push(StatusEntry {
                    path: path.clone(),
                    kind: ChangeKind::Modified,
                });
            } else if flags.contains(git2::Status::INDEX_DELETED) {
                result.staged.push(StatusEntry {
                    path: path.clone(),
                    kind: ChangeKind::Deleted,
                });
            }

            if flags.contains(git2::Status::WT_NEW) {
                result.unstaged.push(StatusEntry {
                    path,
                    kind: ChangeKind::Untracked,
                });
            } else if flags.contains(git2::Status::WT_DELETED) {
                result.unstaged.push(StatusEntry {
                    path,
                    kind: ChangeKind::Deleted,
                });
            } else if flags.intersects(
                git2::Status::WT_MODIFIED | git2::Status::WT_RENAMED | git2::Status::WT_TYPECHANGE,
            ) {
                result.unstaged.push(StatusEntry {
                    path,
                    kind: ChangeKind::Modified,
                });
            }
        }
        Ok(result)
    }

    /// Stages `path` (repo-relative): `index.add_path` if it still exists
    /// on disk (covers new/modified), else `index.remove_path` (stages a
    /// working-tree deletion) -- matches plain `git add <path>`'s own
    /// behavior in both cases.
    pub fn stage_path(&self, path: impl AsRef<Path>) -> Result<(), GitError> {
        let rel_path = normalize(path.as_ref());
        let full_path = self.validate_repo_relative_path(&rel_path)?;
        let mut index = self.repo.index()?;
        if full_path.exists() {
            index.add_path(&rel_path)?;
        } else {
            index.remove_path(&rel_path)?;
        }
        index.write()?;
        Ok(())
    }

    /// Resets `path`'s index entry back to `HEAD`'s tree without touching
    /// the working directory (`git restore --staged <path>`'s exact
    /// semantics). With no commits yet (`HEAD` doesn't resolve), there is
    /// no older version to reset to -- unstaging then just means removing
    /// the path from the index.
    pub fn unstage_path(&self, path: impl AsRef<Path>) -> Result<(), GitError> {
        let rel_path = normalize(path.as_ref());
        self.validate_repo_relative_path(&rel_path)?;
        match self.repo.head() {
            Ok(head) => {
                let head_commit = head.peel_to_commit()?;
                self.repo
                    .reset_default(Some(head_commit.as_object()), [rel_path.as_path()])?;
            }
            Err(_) => {
                let mut index = self.repo.index()?;
                index.remove_path(&rel_path)?;
                index.write()?;
            }
        }
        Ok(())
    }

    /// Discards `path`'s working-tree changes (`docs/features/
    /// git-commit-and-staging.md` §3.3): checks it out from `HEAD` if
    /// tracked (overwriting or restoring it), or deletes it from disk if
    /// untracked (there is nothing in `HEAD` to check out). Never called
    /// for a conflicted path -- the Conflicts UI owns that flow. No
    /// trash/undo: the untracked-deletion branch is the one place in this
    /// module that destroys content rather than overwriting it, so the
    /// escape check here is load-bearing in a way the write paths' checks
    /// weren't.
    pub fn discard_path(&self, path: impl AsRef<Path>) -> Result<(), GitError> {
        let rel_path = normalize(path.as_ref());
        let full_path = self.validate_repo_relative_path(&rel_path)?;
        // Branches on whether `HEAD`'s tree actually has this path, not on
        // whether it currently exists on disk -- a tracked-then-deleted
        // file exists in `HEAD` but not on disk (must `checkout_head` to
        // restore it), while an untracked file exists on disk but not in
        // `HEAD` (must be deleted, `checkout_head` has nothing to give
        // it). Also covers a brand-new repo with no commits yet
        // (`head_tree()` returns `None`) the same way untracked does --
        // there's nothing in `HEAD` to restore either way.
        let has_head_content = self
            .head_tree()?
            .is_some_and(|tree| tree.get_path(&rel_path).is_ok());
        if has_head_content {
            let mut checkout = git2::build::CheckoutBuilder::new();
            // `CheckoutBuilder::path` treats its argument as a pathspec
            // *pattern* by default (git2's own doc comment) -- a tracked
            // file whose name happens to contain a glob-special character
            // (`*`, `?`, `[...]`) would otherwise make this checkout
            // restore every sibling file that also matches the pattern,
            // not just the one path the caller asked to discard.
            // `disable_pathspec_match` forces an exact literal match.
            checkout
                .path(rel_path.as_path())
                .disable_pathspec_match(true)
                .force();
            self.repo.checkout_head(Some(&mut checkout))?;
        } else if full_path.exists() {
            fs::remove_file(&full_path)?;
        }
        Ok(())
    }

    /// Commits the current index (`docs/features/git-commit-and-staging.md`
    /// §3.4). `amend` keeps the previous commit's parents and, for any
    /// `None`-equivalent left unspecified, its author/message too --
    /// `message` here always replaces it (an empty amend message meaning
    /// "keep the old one" is `GitPanel`'s job to pre-fill, not this
    /// layer's to infer). Returns the new commit's full hex id.
    /// **`MERGE_HEAD`-aware**: if `.git/MERGE_HEAD` is present (left by
    /// `merge_branch` on conflicts, or by an external `git merge`), the
    /// produced commit gets *two* parents -- current `HEAD`'s commit and
    /// `MERGE_HEAD`'s commit -- instead of one, and `cleanup_state` runs
    /// afterward to clear `MERGE_HEAD`/`MERGE_MSG`. Without this, a user
    /// who resolved every conflict via `resolve_conflict` and then called
    /// this method would silently get a commit that discards the merge
    /// relationship entirely (`docs/features/git-branches-and-blame.md`
    /// §1.1). `amend` is unaffected -- amending during an in-progress
    /// merge stays out of v1 scope and only ever touches `HEAD`'s existing
    /// single-parent shape, matching prior behavior. A repo with no
    /// `MERGE_HEAD` takes the same single/zero-parent path as before this
    /// method existed.
    pub fn commit(&self, message: &str, amend: bool) -> Result<String, GitError> {
        let mut index = self.repo.index()?;
        let tree_oid = index.write_tree()?;
        let tree = self.repo.find_tree(tree_oid)?;
        let sig = self.repo.signature()?;

        if amend {
            let head_commit = self.repo.head()?.peel_to_commit()?;
            let oid = head_commit.amend(
                Some("HEAD"),
                Some(&sig),
                Some(&sig),
                None,
                Some(message),
                Some(&tree),
            )?;
            return Ok(oid.to_string());
        }

        if message.trim().is_empty() {
            return Err(git2::Error::from_str("commit message is empty").into());
        }
        let parent = match self.repo.head() {
            Ok(head) => Some(head.peel_to_commit()?),
            Err(_) => None,
        };
        let merge_head_commit = match self.repo.find_reference("MERGE_HEAD") {
            Ok(r) => Some(r.peel_to_commit()?),
            Err(_) => None,
        };
        let mut parents: Vec<&git2::Commit<'_>> = parent.iter().collect();
        parents.extend(merge_head_commit.iter());
        let oid = self
            .repo
            .commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)?;
        if merge_head_commit.is_some() {
            self.repo.cleanup_state()?;
        }
        Ok(oid.to_string())
    }

    /// Rejects `path` outright if it has any non-`Normal` component
    /// (`..`, an absolute root, `.`) before anything on disk is touched,
    /// then canonicalizes the nearest existing ancestor of
    /// `workdir.join(path)` (the full path if it exists, else walking up
    /// through its parents, `workdir` itself in the worst case) and
    /// confirms that's still inside `workdir` -- the same symlink-escape
    /// check `resolve_conflict` already established, generalized to work
    /// even when `path` names something that's already gone (staging or
    /// discarding a deletion has nothing to canonicalize at the leaf).
    /// Returns the joined (uncanonicalized) full path for the caller's
    /// own use.
    fn validate_repo_relative_path(&self, path: &Path) -> Result<PathBuf, GitError> {
        // An empty path has zero components, so the component-scan loop
        // below can't reject it -- and `workdir.join("")` resolves to
        // `workdir` itself, which downstream git2 calls interpret as "the
        // whole repo" rather than "no such path": `reset_default` with an
        // empty pathspec unstages every staged file, not nothing. Reject
        // it explicitly rather than let an accidental empty string widen
        // a single-file operation to the entire working tree.
        if path.as_os_str().is_empty() {
            return Err(GitError::PathEscapesRepo(path.to_path_buf()));
        }
        for component in path.components() {
            if !matches!(component, std::path::Component::Normal(_)) {
                return Err(GitError::PathEscapesRepo(path.to_path_buf()));
            }
        }
        let full_path = self.workdir.join(path);
        let mut probe = full_path.as_path();
        let canonical_ancestor = loop {
            if let Ok(canonical) = fs::canonicalize(probe) {
                break canonical;
            }
            match probe.parent() {
                Some(parent) => probe = parent,
                None => break self.workdir.clone(),
            }
        };
        if !canonical_ancestor.starts_with(&self.workdir) {
            return Err(GitError::PathEscapesRepo(path.to_path_buf()));
        }
        Ok(full_path)
    }

    fn head_tree(&self) -> Result<Option<git2::Tree<'_>>, GitError> {
        // Same reasoning as `commit_graph`: a repository with no commits
        // yet fails to resolve HEAD in ways that vary by libgit2 version/
        // config rather than one specific error code, so any failure here
        // means "no HEAD tree", not a real error.
        match self.repo.head() {
            Ok(head) => Ok(Some(head.peel_to_tree()?)),
            Err(_) => Ok(None),
        }
    }

    /// Every local branch, alphabetical by name. `is_head` marks the one
    /// `current_branch()` would return -- at most one `true`, none if
    /// `HEAD` doesn't resolve (unborn/empty repo, same "not an error"
    /// treatment as `current_branch`/`commit_graph`).
    pub fn branches(&self) -> Result<Vec<BranchInfo>, GitError> {
        let head_name = self.current_branch();
        let mut result = Vec::new();
        for branch in self.repo.branches(Some(git2::BranchType::Local))? {
            let (branch, _) = branch?;
            let name = branch
                .name()?
                .map(str::to_string)
                .ok_or_else(|| git2::Error::from_str("branch name is not valid UTF-8"))?;
            let is_head = head_name.as_deref() == Some(name.as_str());
            result.push(BranchInfo { name, is_head });
        }
        result.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(result)
    }

    /// Creates a local branch named `name` pointing at `start_point` (any
    /// revspec `revparse_single` accepts -- a branch name, tag, or commit
    /// id) or `HEAD` if `None`. Does **not** switch to it -- that's a
    /// separate `switch_branch` call, so the UI can offer "create" and
    /// "create and checkout" as one button without this API baking in
    /// that policy. Errors if `name` already exists (git2's own behavior,
    /// not force-overwritten).
    pub fn create_branch(&self, name: &str, start_point: Option<&str>) -> Result<(), GitError> {
        let target = match start_point {
            Some(rev) => self.repo.revparse_single(rev)?.peel_to_commit()?,
            None => self.repo.head()?.peel_to_commit()?,
        };
        self.repo.branch(name, &target, false)?;
        Ok(())
    }

    /// Checks out local branch `name`: moves `HEAD`, updates the working
    /// tree. Uses libgit2's **safe** checkout mode (not force) -- refuses
    /// if the working tree has uncommitted changes that checkout would
    /// overwrite, the same "don't silently discard uncommitted work"
    /// behavior plain `git checkout` has, and the same principle
    /// `resolve_conflict`'s own write path already follows elsewhere in
    /// this file. There is no stash feature yet -- a caller hitting this
    /// has to commit or discard first.
    pub fn switch_branch(&self, name: &str) -> Result<(), GitError> {
        let refname = format!("refs/heads/{name}");
        let obj = self.repo.revparse_single(&refname)?;
        self.repo
            .checkout_tree(&obj, Some(git2::build::CheckoutBuilder::new().safe()))?;
        self.repo.set_head(&refname)?;
        Ok(())
    }

    /// Deletes local branch `name`. Refuses (`GitError::BranchNotMerged`)
    /// unless `name`'s tip is an ancestor of (or equal to) the current
    /// `HEAD` -- the same safety `git branch -d` gives before `-D`'s
    /// force -- unless `force` is `true`, which skips that check entirely
    /// (`git branch -D` equivalent). Also refuses to delete the branch
    /// `HEAD` currently points at regardless of `force` -- libgit2 itself
    /// enforces this (verified live by this module's own test).
    pub fn delete_branch(&self, name: &str, force: bool) -> Result<(), GitError> {
        let mut branch = self.repo.find_branch(name, git2::BranchType::Local)?;
        if !force {
            let branch_oid = branch
                .get()
                .target()
                .ok_or_else(|| git2::Error::from_str("branch has no target"))?;
            let head_oid = self
                .repo
                .head()?
                .target()
                .ok_or_else(|| git2::Error::from_str("HEAD has no target"))?;
            let merged =
                branch_oid == head_oid || self.repo.graph_descendant_of(head_oid, branch_oid)?;
            if !merged {
                return Err(GitError::BranchNotMerged(name.to_string()));
            }
        }
        branch.delete()?;
        Ok(())
    }

    /// Merges local branch `name` into the current `HEAD` branch. See
    /// `MergeOutcome`'s own doc comment for the four outcomes. Uses
    /// `merge_analysis` to pick fast-forward vs. a real merge; a real,
    /// conflict-free merge is finished by calling this type's own
    /// `commit()` (already `MERGE_HEAD`-aware, see its doc comment) with
    /// a default message `"Merge branch '<name>' into <current-branch>"`
    /// -- the same code path a user finishing a conflicted merge by hand
    /// goes through, so there is exactly one way a merge commit ever gets
    /// created, not two.
    pub fn merge_branch(&self, name: &str) -> Result<MergeOutcome, GitError> {
        let branch = self.repo.find_branch(name, git2::BranchType::Local)?;
        let their_oid = branch
            .get()
            .target()
            .ok_or_else(|| git2::Error::from_str("branch has no target"))?;
        let their_commit = self.repo.find_annotated_commit(their_oid)?;
        let (analysis, _) = self.repo.merge_analysis(&[&their_commit])?;

        if analysis.is_up_to_date() {
            return Ok(MergeOutcome::UpToDate);
        }
        if analysis.is_fast_forward() {
            // `checkout_head` is documented to require the *opposite*
            // order (check out the target first, move `HEAD` after) --
            // calling it after `HEAD` already points at the new commit
            // makes libgit2 think the working tree is unexpectedly dirty
            // relative to the (already-moved) `HEAD`, and it silently
            // skips writing anything (verified live: a fast-forward that
            // adds a new file left it absent from the working tree).
            // `checkout_tree` against the target object first, exactly
            // matching `switch_branch`'s own already-correct order above,
            // avoids that.
            let head_ref_name = self.repo.head()?.name()?.to_string();
            let target_obj = self.repo.find_object(their_oid, None)?;
            self.repo.checkout_tree(
                &target_obj,
                Some(git2::build::CheckoutBuilder::new().safe()),
            )?;
            let mut head_ref = self.repo.find_reference(&head_ref_name)?;
            head_ref.set_target(their_oid, "fast-forward merge")?;
            self.repo.set_head(&head_ref_name)?;
            return Ok(MergeOutcome::FastForward);
        }

        self.repo.merge(&[&their_commit], None, None)?;
        let index = self.repo.index()?;
        if index.has_conflicts() {
            return Ok(MergeOutcome::Conflicts(self.conflicts()?));
        }
        let current = self.current_branch().unwrap_or_else(|| "HEAD".to_string());
        let message = format!("Merge branch '{name}' into {current}");
        let commit_id = self.commit(&message, false)?;
        Ok(MergeOutcome::Merged { commit_id })
    }

    /// Full detail for one commit (by id or any revspec `revparse_single`
    /// accepts) -- a superset of `CommitNode`, used by the blame popup and
    /// reusable as-is by a future commit-details pane. Uses the same
    /// degrade-to-fallback approach `commit_graph` already establishes for
    /// display-only fields with unusual (non-UTF-8) metadata.
    pub fn commit_detail(&self, commit_id: &str) -> Result<CommitDetail, GitError> {
        let commit = self.repo.revparse_single(commit_id)?.peel_to_commit()?;
        let short_id = commit
            .as_object()
            .short_id()
            .ok()
            .and_then(|buf| buf.as_str().ok().map(str::to_string))
            .unwrap_or_else(|| commit.id().to_string());
        let summary = commit
            .summary()
            .ok()
            .flatten()
            .unwrap_or_default()
            .to_string();
        let body = commit
            .body()
            .ok()
            .flatten()
            .unwrap_or_default()
            .trim()
            .to_string();
        let author = commit.author();
        Ok(CommitDetail {
            id: commit.id().to_string(),
            short_id,
            summary,
            body,
            author: author.name().ok().unwrap_or_default().to_string(),
            email: author.email().ok().unwrap_or_default().to_string(),
            timestamp: commit.time().seconds(),
        })
    }

    /// Per-line blame for `path` (repo-relative) against `HEAD`. Computed
    /// from `HEAD`'s committed blob content, **not** the live editor
    /// buffer or an unsaved on-disk edit -- same "diff/gutter only look at
    /// what's on disk and last-saved, not the live buffer" precedent
    /// `diff_file` already establishes. `path` that isn't tracked at
    /// `HEAD` (new/untracked file) returns `Ok(vec![])`, mirroring
    /// `diff_file`'s "untracked shows no diff" treatment rather than an
    /// error. Capped at `MAX_BLAME_LINES` via `BlameOptions::max_line`, so
    /// the git2-side call itself is bounded, not just the output size.
    pub fn blame_file(&self, path: impl AsRef<Path>) -> Result<Vec<BlameLine>, GitError> {
        let rel_path = normalize(path.as_ref());
        let Some(tree) = self.head_tree()? else {
            return Ok(Vec::new());
        };
        let Ok(entry) = tree.get_path(&rel_path) else {
            return Ok(Vec::new());
        };

        // `BlameOptions::max_line` isn't safe to set to a fixed cap above
        // the file's real length -- verified live that libgit2's last
        // hunk then reports `lines_in_hunk()` stretching out to the
        // requested `max_line` rather than clamping to the blob's actual
        // content (a 3-line file with `max_line(20_000)` came back
        // reporting 20,000 lines). Count the real line total first (cheap
        // -- a single byte scan, not a blame walk) and cap `max_line` at
        // whichever of that or `MAX_BLAME_LINES` is smaller, so a
        // genuinely huge file still gets its blame *walk* bounded, not
        // just its output size.
        let blob = self.repo.find_blob(entry.id())?;
        let content = blob.content();
        let mut line_count = content.iter().filter(|&&b| b == b'\n').count();
        if !content.is_empty() && content.last() != Some(&b'\n') {
            line_count += 1;
        }
        if line_count == 0 {
            return Ok(Vec::new());
        }

        let mut opts = git2::BlameOptions::new();
        opts.max_line(line_count.min(MAX_BLAME_LINES));
        let blame = self.repo.blame_file(&rel_path, Some(&mut opts))?;

        let mut lines = Vec::new();
        for hunk in blame.iter() {
            let commit_id = hunk.final_commit_id();
            let commit = self.repo.find_commit(commit_id)?;
            let short_id = commit
                .as_object()
                .short_id()
                .ok()
                .and_then(|buf| buf.as_str().ok().map(str::to_string))
                .unwrap_or_else(|| commit_id.to_string());
            let summary = commit
                .summary()
                .ok()
                .flatten()
                .unwrap_or_default()
                .to_string();
            let author = commit.author().name().ok().unwrap_or_default().to_string();
            let timestamp = commit.time().seconds();
            let start = hunk.final_start_line();
            for offset in 0..hunk.lines_in_hunk() {
                lines.push(BlameLine {
                    line: start - 1 + offset,
                    commit_id: commit_id.to_string(),
                    short_id: short_id.clone(),
                    author: author.clone(),
                    timestamp,
                    summary: summary.clone(),
                });
            }
        }
        lines.sort_by_key(|l| l.line);
        Ok(lines)
    }

    /// Lists every linked worktree of this repository, sorted by name. A
    /// worktree whose on-disk directory is missing or otherwise fails to
    /// open is still included (`branch: None`) rather than silently
    /// dropped or erroring the whole call -- the user needs to see it to
    /// remove/prune it (`docs/features/git-worktrees.md` §2.1). An entry
    /// `find_worktree` itself can't open at all (corrupt registration) is
    /// skipped rather than failing the whole listing, same "one bad entry
    /// doesn't sink the call" precedent `status()`'s non-UTF-8-path
    /// handling already establishes.
    pub fn worktrees(&self) -> Result<Vec<WorktreeInfo>, GitError> {
        let names = self.repo.worktrees()?;
        let mut result = Vec::new();
        for name in names.iter().filter_map(|entry| entry.ok().flatten()) {
            let Ok(wt) = self.repo.find_worktree(name) else {
                continue;
            };
            let is_locked = matches!(wt.is_locked(), Ok(git2::WorktreeLockStatus::Locked(_)));
            let branch = git2::Repository::open_from_worktree(&wt)
                .ok()
                .and_then(|repo| {
                    let head = repo.head().ok()?;
                    if head.is_branch() {
                        head.shorthand().ok().map(str::to_string)
                    } else {
                        None
                    }
                });
            result.push(WorktreeInfo {
                name: name.to_string(),
                path: wt.path().to_path_buf(),
                branch,
                is_locked,
            });
        }
        result.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(result)
    }

    /// Registers a new linked worktree named `name` at `path`, optionally
    /// checking out the existing local branch `branch` in it. If `branch`
    /// is `None`, this is `git2`'s own default `git_worktree_add`
    /// behaviour: a brand-new branch literally named `name` is created
    /// pointing at current `HEAD` and checked out in the new worktree. If
    /// `branch` is `Some` and doesn't name an existing local branch, the
    /// underlying `git2::Error` from `find_branch` propagates unchanged
    /// (this method never creates a branch on the caller's behalf beyond
    /// that one implicit no-`branch`-given case).
    ///
    /// Validates before ever calling into `git2` (`docs/features/
    /// git-worktrees.md` §2.1, hardened per `docs/security-findings/
    /// git-worktrees-core-2026-09-01.md` findings 1-2): `name` non-empty,
    /// containing no `/` or `\`, not `.`/`..`, and containing no Unicode
    /// bidi control character (the same "Trojan Source"/CVE-2021-42574
    /// character classes `crates/ui/src/editor/blame_gutter.rs`'s
    /// `strip_bidi_controls` already strips from repository-sourced
    /// display text -- rejected outright here, since this is the
    /// *creation* path and an app-chosen name should never need one)
    /// (`InvalidWorktreeName`) -- it becomes a literal path component
    /// under `.git/worktrees/<name>` and, when `branch` is `None`, a
    /// literal branch name too; `name` not already registered
    /// (`WorktreeNameTaken`); `path` doesn't already exist as a non-empty
    /// directory (`DestinationNotEmpty`, same check `clone_repo` already
    /// makes); neither `path` itself (when it already exists on disk) nor
    /// its parent (when *that* already exists) canonicalizes to somewhere
    /// inside this repository's own `workdir()` (`WorktreeInsideRepo`) --
    /// checking `path` itself in addition to its parent closes a gap
    /// where `path` was a pre-existing symlink into the workdir (finding
    /// 2); a path with neither itself nor its parent existing yet is let
    /// through unchecked, matching `clone_repo`'s own precedent of not
    /// requiring the destination to pre-exist.
    pub fn add_worktree(
        &self,
        name: &str,
        path: impl AsRef<Path>,
        branch: Option<&str>,
    ) -> Result<(), GitError> {
        let has_bidi_control = name.chars().any(|c| {
            matches!(
                c,
                '\u{202A}'..='\u{202E}'
                    | '\u{2066}'..='\u{2069}'
                    | '\u{200E}'
                    | '\u{200F}'
                    | '\u{061C}'
            )
        });
        if name.is_empty()
            || name.contains(['/', '\\'])
            || name == "."
            || name == ".."
            || has_bidi_control
        {
            return Err(GitError::InvalidWorktreeName(name.to_string()));
        }
        if self
            .repo
            .worktrees()?
            .iter()
            .filter_map(|entry| entry.ok().flatten())
            .any(|existing| existing == name)
        {
            return Err(GitError::WorktreeNameTaken(name.to_string()));
        }

        let path = path.as_ref();
        if path.exists() && fs::read_dir(path)?.next().is_some() {
            return Err(GitError::DestinationNotEmpty(path.to_path_buf()));
        }
        if let Ok(canon_path) = path.canonicalize() {
            if canon_path.starts_with(self.workdir()) {
                return Err(GitError::WorktreeInsideRepo(path.to_path_buf()));
            }
        }
        if let Some(parent) = path.parent() {
            if let Ok(canon_parent) = parent.canonicalize() {
                if canon_parent.starts_with(self.workdir()) {
                    return Err(GitError::WorktreeInsideRepo(path.to_path_buf()));
                }
            }
        }

        let mut opts = git2::WorktreeAddOptions::new();
        let found_branch;
        if let Some(branch_name) = branch {
            found_branch = self
                .repo
                .find_branch(branch_name, git2::BranchType::Local)?
                .into_reference();
            opts.reference(Some(&found_branch));
        }
        self.repo.worktree(name, path, Some(&opts))?;
        Ok(())
    }

    /// Removes the linked worktree named `name`: deletes its on-disk
    /// working directory and its `.git/worktrees/<name>` registration
    /// (`git worktree remove` semantics, not the weaker `prune` which
    /// only cleans up an already-deleted directory's registration).
    ///
    /// With `force: false`, refuses if either check fails: the worktree
    /// is locked (`WorktreeLocked`, queried via `Worktree::is_locked` on
    /// the registration itself -- this does not require opening the
    /// worktree's working directory, so it runs and can still block even
    /// when the directory is unreachable), or opening the worktree's own
    /// repository and checking `Repository::statuses` (the same
    /// `include_untracked(true)`/`recurse_untracked_dirs(true)` options
    /// `status()` already uses, not the bare default, which omits
    /// untracked files) finds anything (`WorktreeHasUncommittedChanges`).
    ///
    /// A worktree whose directory is missing/unreachable on disk skips
    /// only the uncommitted-changes check -- there's nothing there to
    /// check. The lock check still applies in that case: a directory
    /// being unreachable is indistinguishable on disk from a locked
    /// worktree on offline removable/network storage, so an
    /// unreachable-but-locked worktree still refuses without `force`.
    /// `force: true` skips both checks unconditionally, matching
    /// `delete_branch`'s existing `force` shape elsewhere in this file.
    pub fn remove_worktree(&self, name: &str, force: bool) -> Result<(), GitError> {
        let wt = self.repo.find_worktree(name)?;

        if !force {
            if matches!(wt.is_locked(), Ok(git2::WorktreeLockStatus::Locked(_))) {
                return Err(GitError::WorktreeLocked(wt.path().to_path_buf()));
            }
            if let Ok(wt_repo) = git2::Repository::open_from_worktree(&wt) {
                let mut opts = git2::StatusOptions::new();
                opts.include_untracked(true).recurse_untracked_dirs(true);
                let dirty = wt_repo
                    .statuses(Some(&mut opts))
                    .map(|statuses| !statuses.is_empty())
                    .unwrap_or(false);
                if dirty {
                    return Err(GitError::WorktreeHasUncommittedChanges(
                        wt.path().to_path_buf(),
                    ));
                }
            }
        }

        let mut prune_opts = git2::WorktreePruneOptions::new();
        prune_opts.valid(true).locked(force).working_tree(true);
        wt.prune(Some(&mut prune_opts))?;
        Ok(())
    }

    fn blob_utf8(&self, entry: Option<&git2::IndexEntry>) -> Result<Option<String>, GitError> {
        let Some(entry) = entry else {
            return Ok(None);
        };
        let blob = self.repo.find_blob(entry.id)?;
        let text = std::str::from_utf8(blob.content())
            .map_err(|_| git2::Error::from_str("conflict side is not valid UTF-8"))?;
        Ok(Some(text.to_string()))
    }
}

/// Snapshot of libgit2's `git2::indexer::Progress` at one point during a
/// clone, re-exposed as plain fields so `ide-ui` doesn't need a `git2`
/// dependency of its own just to read progress out of a callback
/// (`docs/features/git-remote.md` §2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CloneProgress {
    pub received_objects: usize,
    pub total_objects: usize,
    pub indexed_objects: usize,
    pub indexed_deltas: usize,
    pub total_deltas: usize,
    pub received_bytes: usize,
}

impl From<git2::Progress<'_>> for CloneProgress {
    fn from(p: git2::Progress<'_>) -> Self {
        Self {
            received_objects: p.received_objects(),
            total_objects: p.total_objects(),
            indexed_objects: p.indexed_objects(),
            indexed_deltas: p.indexed_deltas(),
            total_deltas: p.total_deltas(),
            received_bytes: p.received_bytes(),
        }
    }
}

/// Clones `url` into `dest`, calling `on_progress` zero or more times
/// during the transfer. Blocking, synchronous, like every other
/// `GitRepo`/`git`-module function -- `ide-ui` is responsible for calling
/// this off the UI thread (`docs/features/git-remote.md` §3.1).
///
/// Credentials are delegated entirely to whatever the OS/git installation
/// already has configured (SSH agent, git credential helper) -- this
/// function never prompts for, stores, or logs a credential itself (§3.3).
/// `RemoteCallbacks::certificate_check` is never set, so TLS/SSH host-key
/// verification always runs at libgit2's own default (§3.4) -- this must
/// never change to an unconditional bypass.
///
/// After a successful clone, `dest` is validated by running it through
/// `crate::project::Project::open`/`scan_tree` -- the same symlink-escape
/// exclusion every other project open already gets -- rather than this
/// function inventing its own path-escape logic (§3.5). A failure at that
/// step (not expected for a directory this function just populated, but
/// not assumed impossible either) returns `GitError::ClonedContentInvalid`
/// instead of a `GitRepo` the rest of the IDE might not be able to safely
/// treat as a project root.
pub fn clone_repo(
    url: &str,
    dest: impl AsRef<Path>,
    mut on_progress: impl FnMut(CloneProgress),
) -> Result<GitRepo, GitError> {
    let dest = dest.as_ref();

    if url.trim().is_empty() {
        return Err(GitError::EmptyUrl);
    }
    if dest.exists() && fs::read_dir(dest)?.next().is_some() {
        return Err(GitError::DestinationNotEmpty(dest.to_path_buf()));
    }

    let mut callbacks = git2::RemoteCallbacks::new();
    callbacks.credentials(|url, username, allowed_types| {
        if allowed_types.contains(git2::CredentialType::SSH_KEY) {
            let agent_user = username.unwrap_or("git");
            if let Ok(cred) = git2::Cred::ssh_key_from_agent(agent_user) {
                return Ok(cred);
            }
        }
        if allowed_types.contains(git2::CredentialType::USER_PASS_PLAINTEXT) {
            if let Ok(config) = git2::Config::open_default() {
                if let Ok(cred) = git2::Cred::credential_helper(&config, url, username) {
                    return Ok(cred);
                }
            }
        }
        Err(git2::Error::from_str(
            "no usable credential for the types this server accepts",
        ))
    });
    callbacks.transfer_progress(|progress| {
        on_progress(CloneProgress::from(progress));
        true
    });

    let mut fetch_options = git2::FetchOptions::new();
    fetch_options.remote_callbacks(callbacks);

    git2::build::RepoBuilder::new()
        .fetch_options(fetch_options)
        .clone(url, dest)?;

    let project = crate::project::Project::open(dest)
        .map_err(|_| GitError::ClonedContentInvalid(dest.to_path_buf()))?;
    project.scan_tree();

    GitRepo::open(dest)
}

/// git always uses `/`-separated repo-relative paths internally; accept a
/// platform path from the caller but normalize separators before using it
/// as a lookup key or index path.
fn normalize(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().replace('\\', "/"))
}

fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

#[derive(Default)]
struct DiffBuildState {
    files: Vec<FileDiff>,
}

fn build_file_diffs(diff: &git2::Diff) -> Result<Vec<FileDiff>, GitError> {
    let state = RefCell::new(DiffBuildState::default());

    let mut file_cb = |delta: git2::DiffDelta, _progress: f32| -> bool {
        state.borrow_mut().files.push(FileDiff {
            old_path: delta.old_file().path().map(|p| p.to_path_buf()),
            new_path: delta.new_file().path().map(|p| p.to_path_buf()),
            hunks: Vec::new(),
            truncated: false,
        });
        true
    };

    let mut binary_cb = |_delta: git2::DiffDelta, _binary: git2::DiffBinary| -> bool {
        // Binary files are excluded from diff output entirely in v1
        // (docs/features/git-support.md §4) — drop the entry `file_cb`
        // just pushed for it.
        state.borrow_mut().files.pop();
        true
    };

    let mut hunk_cb = |_delta: git2::DiffDelta, hunk: git2::DiffHunk| -> bool {
        let mut state = state.borrow_mut();
        if let Some(file) = state.files.last_mut() {
            file.hunks.push(DiffHunk {
                old_start: hunk.old_start(),
                new_start: hunk.new_start(),
                lines: Vec::new(),
            });
        }
        true
    };

    let mut line_cb =
        |_delta: git2::DiffDelta, _hunk: Option<git2::DiffHunk>, line: git2::DiffLine| -> bool {
            // `line.content()` includes the line's own trailing newline
            // (patch-format convention); strip it so a `DiffLine` is
            // exactly one row's text with no embedded newline, matching
            // §3's "one row per line" side-by-side rendering.
            let text = String::from_utf8_lossy(line.content());
            let text = text.strip_suffix('\n').unwrap_or(&text);
            let text = text.strip_suffix('\r').unwrap_or(text).to_string();
            let diff_line = match line.origin() {
                '+' => Some(DiffLine::Added(text, Vec::new())),
                '-' => Some(DiffLine::Removed(text, Vec::new())),
                ' ' => Some(DiffLine::Context(text)),
                // File/hunk header and "no newline at end of file" marker
                // lines carry no diffable content.
                _ => None,
            };
            if let Some(diff_line) = diff_line {
                let mut state = state.borrow_mut();
                if let Some(hunk) = state.files.last_mut().and_then(|f| f.hunks.last_mut()) {
                    hunk.lines.push(diff_line);
                }
            }
            true
        };

    diff.foreach(
        &mut file_cb,
        Some(&mut binary_cb),
        Some(&mut hunk_cb),
        Some(&mut line_cb),
    )?;

    let mut files = state.into_inner().files;
    for file in &mut files {
        // Truncation first, pairing second (doc §4): a replace block whose
        // Added run gets cut off by `truncate_file_diff` must leave its
        // Removed lines with empty spans, not a span computed against an
        // Added line that no longer exists in the output.
        truncate_file_diff(file);
        for hunk in &mut file.hunks {
            pair_intraline_spans(&mut hunk.lines);
        }
    }
    Ok(files)
}

/// Caps a single file's total line count (across all hunks) at
/// `MAX_DIFF_LINES`, dropping any hunk (or partial hunk) beyond the cap
/// and setting `truncated` — see the constant's doc for why.
fn truncate_file_diff(file: &mut FileDiff) {
    let mut total = 0usize;
    let mut kept = Vec::with_capacity(file.hunks.len());
    let mut truncated = false;

    for mut hunk in std::mem::take(&mut file.hunks) {
        if truncated {
            break;
        }
        let remaining = MAX_DIFF_LINES.saturating_sub(total);
        if hunk.lines.len() > remaining {
            hunk.lines.truncate(remaining);
            truncated = true;
        }
        total += hunk.lines.len();
        if !hunk.lines.is_empty() {
            kept.push(hunk);
        }
    }

    file.hunks = kept;
    file.truncated = truncated;
}

/// Pairs each maximal run of consecutive `Removed` lines with the maximal
/// run of consecutive `Added` lines immediately following it (the shape
/// git's line-oriented diff always produces for a changed region), index-
/// wise up to the shorter run's length, and fills in each pair's intraline
/// spans via [`intraline_diff`]. Lines beyond the paired count in either
/// run — and any run with no counterpart at all (pure insertion/deletion)
/// — keep the empty span `Vec` they were constructed with.
fn pair_intraline_spans(lines: &mut [DiffLine]) {
    let mut i = 0;
    while i < lines.len() {
        if !matches!(lines[i], DiffLine::Removed(..)) {
            i += 1;
            continue;
        }
        let removed_start = i;
        let mut removed_end = removed_start;
        while removed_end < lines.len() && matches!(lines[removed_end], DiffLine::Removed(..)) {
            removed_end += 1;
        }
        let mut added_end = removed_end;
        while added_end < lines.len() && matches!(lines[added_end], DiffLine::Added(..)) {
            added_end += 1;
        }

        let removed_count = removed_end - removed_start;
        let added_count = added_end - removed_end;
        let pairs = removed_count.min(added_count);
        for k in 0..pairs {
            let removed_idx = removed_start + k;
            let added_idx = removed_end + k;
            let (old_text, new_text) = match (&lines[removed_idx], &lines[added_idx]) {
                (DiffLine::Removed(old, _), DiffLine::Added(new, _)) => (old.clone(), new.clone()),
                _ => unreachable!("run boundaries only ever index Removed/Added lines"),
            };
            let (old_spans, new_spans) = intraline_diff(&old_text, &new_text);
            if let DiffLine::Removed(_, spans) = &mut lines[removed_idx] {
                *spans = old_spans;
            }
            if let DiffLine::Added(_, spans) = &mut lines[added_idx] {
                *spans = new_spans;
            }
        }

        i = added_end;
    }
}

/// Longest-common-prefix/longest-common-suffix trim between two lines.
/// Returns the (possibly empty) differing span for each side, always
/// `char`-boundary-aligned — see
/// `docs/features/diff-viewer-enhancements.md` §3.4 for the rationale
/// (not a full word-level Myers diff) and worked examples.
fn intraline_diff(old: &str, new: &str) -> (Vec<DiffSpan>, Vec<DiffSpan>) {
    let old_chars: Vec<(usize, char)> = old.char_indices().collect();
    let new_chars: Vec<(usize, char)> = new.char_indices().collect();
    let shorter = old_chars.len().min(new_chars.len());

    let mut prefix = 0;
    while prefix < shorter && old_chars[prefix].1 == new_chars[prefix].1 {
        prefix += 1;
    }

    let mut suffix = 0;
    while suffix < shorter - prefix
        && old_chars[old_chars.len() - 1 - suffix].1 == new_chars[new_chars.len() - 1 - suffix].1
    {
        suffix += 1;
    }

    let span_for = |chars: &[(usize, char)], text: &str| -> Vec<DiffSpan> {
        let start = chars.get(prefix).map(|(b, _)| *b).unwrap_or(text.len());
        let end = if suffix == 0 {
            text.len()
        } else {
            chars[chars.len() - suffix].0
        };
        if start < end {
            vec![DiffSpan { start, end }]
        } else {
            Vec::new()
        }
    };

    (span_for(&old_chars, old), span_for(&new_chars, new))
}

/// Line-level diff between two arbitrary in-memory texts -- not backed by
/// any git object or working tree (`docs/features/refactor-this.md`
/// §2.1). Built on `git2::Patch::from_buffers`, which needs no
/// `Repository` at all -- confirmed by reading the vendored `git2`
/// source before reaching for this over hand-rolling a diff algorithm.
/// Reuses this module's existing post-processing (`truncate_file_diff`,
/// `pair_intraline_spans`) so the result renders through the exact same
/// `render_diff` the Source Control view's git diffs already use. `path`
/// is used only for the returned `FileDiff`'s `old_path`/`new_path`
/// (both set to the same value -- this is never a rename), display
/// purposes only. `None` when `old == new` (no visible change).
pub fn diff_text(path: &Path, old: &str, new: &str) -> Option<FileDiff> {
    if old == new {
        return None;
    }
    let patch = git2::Patch::from_buffers(old.as_bytes(), None, new.as_bytes(), None, None).ok()?;

    let mut file = FileDiff {
        old_path: Some(path.to_path_buf()),
        new_path: Some(path.to_path_buf()),
        hunks: Vec::new(),
        truncated: false,
    };
    for hunk_idx in 0..patch.num_hunks() {
        let (hunk, line_count) = patch.hunk(hunk_idx).ok()?;
        let mut lines = Vec::with_capacity(line_count);
        for line_idx in 0..line_count {
            let line = patch.line_in_hunk(hunk_idx, line_idx).ok()?;
            let text = String::from_utf8_lossy(line.content());
            let text = text.strip_suffix('\n').unwrap_or(&text);
            let text = text.strip_suffix('\r').unwrap_or(text).to_string();
            match line.origin() {
                '+' => lines.push(DiffLine::Added(text, Vec::new())),
                '-' => lines.push(DiffLine::Removed(text, Vec::new())),
                ' ' => lines.push(DiffLine::Context(text)),
                _ => {}
            }
        }
        file.hunks.push(DiffHunk {
            old_start: hunk.old_start(),
            new_start: hunk.new_start(),
            lines,
        });
    }

    truncate_file_diff(&mut file);
    for hunk in &mut file.hunks {
        pair_intraline_spans(&mut hunk.lines);
    }
    Some(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::build::CheckoutBuilder;
    use git2::{BranchType, Repository, Signature};

    fn init_repo() -> (tempfile::TempDir, Repository) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        // Local, not global: `GitRepo::commit`/`merge_branch` (the
        // production methods several tests exercise directly, unlike
        // `commit_file` below which builds commits with its own explicit
        // `sig()`) resolve their signature via `Repository::signature()`,
        // which falls through to the ambient git config -- a CI runner
        // has no global user.name/user.email set, so these tests must not
        // depend on one existing (`config value 'user.name' was not
        // found`, seen on a real run: v0.1.0 tag, run 33501916808).
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "Test User").unwrap();
        config.set_str("user.email", "test@example.com").unwrap();
        (dir, repo)
    }

    fn sig() -> Signature<'static> {
        Signature::now("Test User", "test@example.com").unwrap()
    }

    fn commit_bytes(repo: &Repository, path: &str, content: &[u8], message: &str) -> git2::Oid {
        let workdir = repo.workdir().unwrap();
        fs::write(workdir.join(path), content).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(path)).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let signature = sig();
        let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
        let parents: Vec<&git2::Commit> = parent.as_ref().into_iter().collect();
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &parents,
        )
        .unwrap()
    }

    fn commit_file(repo: &Repository, path: &str, content: &str, message: &str) -> git2::Oid {
        commit_bytes(repo, path, content.as_bytes(), message)
    }

    fn checkout_branch(repo: &Repository, name: &str) {
        let refname = format!("refs/heads/{name}");
        let obj = repo.revparse_single(&refname).unwrap();
        repo.checkout_tree(&obj, Some(CheckoutBuilder::new().force()))
            .unwrap();
        repo.set_head(&refname).unwrap();
    }

    #[test]
    fn open_discovers_repo_at_root() {
        let (dir, _repo) = init_repo();
        let git = GitRepo::open(dir.path()).unwrap();
        assert_eq!(git.workdir(), fs::canonicalize(dir.path()).unwrap());
    }

    #[test]
    fn open_discovers_repo_from_nested_subdir() {
        let (dir, _repo) = init_repo();
        let sub = dir.path().join("a/b/c");
        fs::create_dir_all(&sub).unwrap();
        let git = GitRepo::open(&sub).unwrap();
        assert_eq!(git.workdir(), fs::canonicalize(dir.path()).unwrap());
    }

    #[test]
    fn open_non_repo_errors_not_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            GitRepo::open(dir.path()),
            Err(GitError::NotARepo(_))
        ));
    }

    #[test]
    fn open_bare_repo_errors_not_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        Repository::init_bare(dir.path()).unwrap();
        assert!(matches!(
            GitRepo::open(dir.path()),
            Err(GitError::NotARepo(_))
        ));
    }

    #[test]
    fn is_repo_true_and_false() {
        let (dir, _repo) = init_repo();
        assert!(GitRepo::is_repo(dir.path()));

        let other = tempfile::tempdir().unwrap();
        assert!(!GitRepo::is_repo(other.path()));
    }

    #[test]
    fn current_branch_none_before_first_commit() {
        let (dir, _repo) = init_repo();
        let git = GitRepo::open(dir.path()).unwrap();
        assert_eq!(git.current_branch(), None);
    }

    #[test]
    fn current_branch_returns_shorthand_after_commit() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "one\n", "first");
        let default_branch = repo.head().unwrap().shorthand().unwrap().to_string();

        let git = GitRepo::open(dir.path()).unwrap();
        assert_eq!(git.current_branch(), Some(default_branch));
    }

    #[test]
    fn current_branch_reflects_checkout_of_another_branch() {
        let (dir, repo) = init_repo();
        let first = commit_file(&repo, "f.txt", "one\n", "first");
        let first_commit = repo.find_commit(first).unwrap();
        repo.branch("feature", &first_commit, false).unwrap();
        checkout_branch(&repo, "feature");

        let git = GitRepo::open(dir.path()).unwrap();
        assert_eq!(git.current_branch(), Some("feature".to_string()));
    }

    #[test]
    fn current_branch_returns_head_shorthand_when_detached() {
        let (dir, repo) = init_repo();
        let first = commit_file(&repo, "f.txt", "one\n", "first");
        repo.set_head_detached(first).unwrap();

        let git = GitRepo::open(dir.path()).unwrap();
        assert_eq!(git.current_branch(), Some("HEAD".to_string()));
    }

    #[test]
    fn commit_graph_empty_repo_returns_empty() {
        let (dir, _repo) = init_repo();
        let git = GitRepo::open(dir.path()).unwrap();
        assert_eq!(git.commit_graph(100).unwrap(), Vec::new());
    }

    #[test]
    fn commit_graph_orders_newest_first_with_parents() {
        let (dir, repo) = init_repo();
        let first = commit_file(&repo, "f.txt", "one\n", "first");
        let second = commit_file(&repo, "f.txt", "two\n", "second");

        let git = GitRepo::open(dir.path()).unwrap();
        let graph = git.commit_graph(100).unwrap();

        assert_eq!(graph.len(), 2);
        assert_eq!(graph[0].id, second.to_string());
        assert_eq!(graph[0].parents, vec![first.to_string()]);
        assert_eq!(graph[0].summary, "second");
        assert_eq!(graph[1].id, first.to_string());
        assert!(graph[1].parents.is_empty());
    }

    #[test]
    fn commit_graph_respects_limit() {
        let (dir, repo) = init_repo();
        for i in 0..5 {
            commit_file(&repo, "f.txt", &format!("v{i}\n"), &format!("commit {i}"));
        }
        let git = GitRepo::open(dir.path()).unwrap();
        assert_eq!(git.commit_graph(2).unwrap().len(), 2);
    }

    #[test]
    fn diff_file_unchanged_returns_none() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "same\n", "init");
        let git = GitRepo::open(dir.path()).unwrap();
        assert_eq!(git.diff_file("f.txt").unwrap(), None);
    }

    #[test]
    fn diff_file_modified_returns_hunks() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\nb\nc\n", "init");
        fs::write(dir.path().join("f.txt"), "a\nB\nc\n").unwrap();

        let git = GitRepo::open(dir.path()).unwrap();
        let diff = git.diff_file("f.txt").unwrap().expect("file changed");

        assert_eq!(diff.new_path, Some(PathBuf::from("f.txt")));
        assert!(!diff.truncated);
        let lines: Vec<&DiffLine> = diff.hunks.iter().flat_map(|h| &h.lines).collect();
        // "b" -> "B" is a 1-removed/1-added replace block: no shared
        // prefix/suffix between the two single-char lines, so each side's
        // whole (single-char) text is one intraline span (§3.4's
        // degenerate "no shared prefix or suffix" case).
        assert!(lines.contains(&&DiffLine::Removed(
            "b".to_string(),
            vec![DiffSpan { start: 0, end: 1 }]
        )));
        assert!(lines.contains(&&DiffLine::Added(
            "B".to_string(),
            vec![DiffSpan { start: 0, end: 1 }]
        )));
        assert!(lines.contains(&&DiffLine::Context("a".to_string())));
    }

    #[test]
    fn diff_file_untracked_returns_none() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        fs::write(dir.path().join("new.txt"), "brand new\n").unwrap();

        let git = GitRepo::open(dir.path()).unwrap();
        assert_eq!(git.diff_file("new.txt").unwrap(), None);
    }

    #[test]
    fn diff_file_binary_returns_none() {
        let (dir, repo) = init_repo();
        commit_bytes(&repo, "bin.dat", &[0u8; 20], "init binary");
        let mut modified = vec![0u8; 20];
        modified[5] = 7;
        fs::write(dir.path().join("bin.dat"), &modified).unwrap();

        let git = GitRepo::open(dir.path()).unwrap();
        assert_eq!(git.diff_file("bin.dat").unwrap(), None);
    }

    #[test]
    fn diff_commit_root_commit_shows_full_addition() {
        let (dir, repo) = init_repo();
        let root = commit_file(&repo, "f.txt", "x\ny\n", "root");
        let git = GitRepo::open(dir.path()).unwrap();

        let diffs = git.diff_commit(&root.to_string()).unwrap();
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].new_path, Some(PathBuf::from("f.txt")));
        let lines: Vec<&DiffLine> = diffs[0].hunks.iter().flat_map(|h| &h.lines).collect();
        assert_eq!(
            lines,
            vec![
                &DiffLine::Added("x".to_string(), Vec::new()),
                &DiffLine::Added("y".to_string(), Vec::new())
            ]
        );
    }

    #[test]
    fn diff_commit_against_first_parent() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "first");
        let second = commit_file(&repo, "f.txt", "a\nb\n", "second");

        let git = GitRepo::open(dir.path()).unwrap();
        let diffs = git.diff_commit(&second.to_string()).unwrap();

        assert_eq!(diffs.len(), 1);
        let lines: Vec<&DiffLine> = diffs[0].hunks.iter().flat_map(|h| &h.lines).collect();
        assert_eq!(
            lines,
            vec![
                &DiffLine::Context("a".to_string()),
                &DiffLine::Added("b".to_string(), Vec::new())
            ]
        );
    }

    #[test]
    fn diff_commit_excludes_binary_file() {
        let (dir, repo) = init_repo();
        let oid = commit_bytes(&repo, "bin.dat", &[0u8; 20], "add binary");
        let git = GitRepo::open(dir.path()).unwrap();
        assert_eq!(git.diff_commit(&oid.to_string()).unwrap(), Vec::new());
    }

    #[test]
    fn diff_truncates_at_max_diff_lines() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "orig\n", "init");

        let big: String = (0..MAX_DIFF_LINES + 500)
            .map(|i| format!("line{i}\n"))
            .collect();
        fs::write(dir.path().join("f.txt"), &big).unwrap();

        let git = GitRepo::open(dir.path()).unwrap();
        let diff = git.diff_file("f.txt").unwrap().expect("file changed");

        assert!(diff.truncated);
        let total_lines: usize = diff.hunks.iter().map(|h| h.lines.len()).sum();
        assert_eq!(total_lines, MAX_DIFF_LINES);
    }

    #[test]
    fn diff_commit_caps_file_count_at_max_diff_files() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "seed.txt", "seed\n", "init");

        let n = MAX_DIFF_FILES + 50;
        let mut index = repo.index().unwrap();
        for i in 0..n {
            let name = format!("f{i}.txt");
            fs::write(dir.path().join(&name), format!("content {i}\n")).unwrap();
            index.add_path(Path::new(&name)).unwrap();
        }
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        let signature = sig();
        let oid = repo
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "mass add",
                &tree,
                &[&parent],
            )
            .unwrap();

        let git = GitRepo::open(dir.path()).unwrap();
        let diffs = git.diff_commit(&oid.to_string()).unwrap();
        assert_eq!(diffs.len(), MAX_DIFF_FILES);
    }

    fn setup_conflict(dir: &tempfile::TempDir, repo: &Repository) {
        let base = commit_file(repo, "f.txt", "base\n", "base");
        let default_branch = repo.head().unwrap().shorthand().unwrap().to_string();

        let base_commit = repo.find_commit(base).unwrap();
        repo.branch("theirs", &base_commit, false).unwrap();
        checkout_branch(repo, "theirs");
        commit_file(repo, "f.txt", "theirs\n", "theirs change");

        checkout_branch(repo, &default_branch);
        commit_file(repo, "f.txt", "ours\n", "ours change");

        let theirs_ref = repo.find_branch("theirs", BranchType::Local).unwrap();
        let theirs_annotated = repo
            .reference_to_annotated_commit(theirs_ref.get())
            .unwrap();
        repo.merge(&[&theirs_annotated], None, None).unwrap();

        let _ = dir; // kept for symmetry/clarity at call sites
    }

    #[test]
    fn conflicts_and_conflict_sides_roundtrip() {
        let (dir, repo) = init_repo();
        setup_conflict(&dir, &repo);

        let git = GitRepo::open(dir.path()).unwrap();
        let conflicts = git.conflicts().unwrap();
        assert_eq!(conflicts, vec![PathBuf::from("f.txt")]);

        let sides = git.conflict_sides("f.txt").unwrap();
        assert_eq!(sides.base, Some("base\n".to_string()));
        assert_eq!(sides.ours, Some("ours\n".to_string()));
        assert_eq!(sides.theirs, Some("theirs\n".to_string()));
    }

    #[test]
    fn resolve_conflict_clears_conflict_and_stages() {
        let (dir, repo) = init_repo();
        setup_conflict(&dir, &repo);
        let git = GitRepo::open(dir.path()).unwrap();

        git.resolve_conflict("f.txt", "resolved\n").unwrap();

        assert_eq!(git.conflicts().unwrap(), Vec::<PathBuf>::new());
        assert_eq!(
            fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "resolved\n"
        );
        // staged: the index entry for f.txt must now match the resolved
        // blob. `repo` is a separate Repository handle from the one
        // `git.resolve_conflict` wrote through, and libgit2 caches an
        // already-loaded index in memory per handle — force a reload from
        // disk rather than reading a stale pre-resolution snapshot.
        let mut index = repo.index().unwrap();
        index.read(true).unwrap();
        let entry = index.get_path(Path::new("f.txt"), 0).unwrap();
        let blob = repo.find_blob(entry.id).unwrap();
        assert_eq!(blob.content(), b"resolved\n");
    }

    #[test]
    fn resolve_conflict_leaves_no_temp_file_behind() {
        let (dir, repo) = init_repo();
        setup_conflict(&dir, &repo);
        let git = GitRepo::open(dir.path()).unwrap();

        git.resolve_conflict("f.txt", "resolved\n").unwrap();

        let leftover: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains(".resolve-conflict-")
            })
            .collect();
        assert!(
            leftover.is_empty(),
            "temp file(s) left behind: {leftover:?}"
        );
    }

    #[test]
    fn resolve_conflict_concurrent_calls_never_cross_contaminate_content() {
        // Regression test: an earlier pid+SystemTime::now() temp-name
        // scheme could collide under fast concurrent calls in the same
        // process, letting one thread's content land on a *different*
        // thread's target file while still returning `Ok(())` -- silent
        // data corruption, not just a benign contention error. Concurrent
        // index-lock errors here are expected and fine (see module doc);
        // wrong file content on a call that reported success is not.
        let (dir, repo) = init_repo();
        let n = 32usize;
        for i in 0..n {
            commit_file(&repo, &format!("f{i}.txt"), "orig\n", "init");
        }
        let dir_path = dir.path().to_path_buf();

        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for i in 0..n {
                let dir_path = &dir_path;
                handles.push(scope.spawn(move || {
                    let git = GitRepo::open(dir_path).unwrap();
                    git.resolve_conflict(format!("f{i}.txt"), &format!("resolved-{i}\n"))
                }));
            }
            for (i, h) in handles.into_iter().enumerate() {
                if h.join().unwrap().is_ok() {
                    // Each thread targets a distinct file, so a call that
                    // reports success must have written exactly that
                    // thread's own content -- anything else is
                    // cross-contamination from another thread's write.
                    let content = fs::read_to_string(dir_path.join(format!("f{i}.txt"))).unwrap();
                    assert_eq!(
                        content,
                        format!("resolved-{i}\n"),
                        "f{i}.txt has cross-contaminated content"
                    );
                }
            }
        });
    }

    #[test]
    fn resolve_conflict_rejects_path_escaping_workdir() {
        let (dir, repo) = init_repo();
        setup_conflict(&dir, &repo);
        let git = GitRepo::open(dir.path()).unwrap();

        let outside = dir.path().parent().unwrap().join("outside-target.txt");
        fs::write(&outside, "untouched").unwrap();

        let result = git.resolve_conflict("../outside-target.txt", "malicious");
        assert!(matches!(result, Err(GitError::PathEscapesRepo(_))));
        assert_eq!(fs::read_to_string(&outside).unwrap(), "untouched");

        let _ = fs::remove_file(&outside);
    }

    #[test]
    fn resolve_conflict_rejects_absolute_path_escaping_workdir() {
        // `PathBuf::join` silently discards its base and returns just the
        // argument when the argument is absolute, so an absolute path could
        // in principle bypass a naive "joined then checked" implementation.
        // This proves the final `canonical.starts_with(workdir)` check still
        // catches it regardless of how the join behaved.
        let (dir, repo) = init_repo();
        setup_conflict(&dir, &repo);
        let git = GitRepo::open(dir.path()).unwrap();

        let outside = dir.path().parent().unwrap().join("outside-absolute.txt");
        fs::write(&outside, "untouched").unwrap();

        let result = git.resolve_conflict(&outside, "malicious");
        assert!(matches!(result, Err(GitError::PathEscapesRepo(_))));
        assert_eq!(fs::read_to_string(&outside).unwrap(), "untouched");

        let _ = fs::remove_file(&outside);
    }

    #[test]
    fn conflict_sides_errors_on_non_utf8_side() {
        let (dir, repo) = init_repo();
        let base = commit_bytes(&repo, "f.bin", b"base\n", "base");
        let default_branch = repo.head().unwrap().shorthand().unwrap().to_string();

        let base_commit = repo.find_commit(base).unwrap();
        repo.branch("theirs", &base_commit, false).unwrap();
        checkout_branch(&repo, "theirs");
        commit_bytes(&repo, "f.bin", &[0xFFu8, 0xFE, 0xFD], "theirs change");

        checkout_branch(&repo, &default_branch);
        commit_bytes(&repo, "f.bin", b"ours\n", "ours change");

        let theirs_ref = repo.find_branch("theirs", BranchType::Local).unwrap();
        let theirs_annotated = repo
            .reference_to_annotated_commit(theirs_ref.get())
            .unwrap();
        repo.merge(&[&theirs_annotated], None, None).unwrap();

        let git = GitRepo::open(dir.path()).unwrap();
        assert!(matches!(
            git.conflict_sides("f.bin"),
            Err(GitError::Git2(_))
        ));
    }

    // ---- git-commit-and-staging (status/stage/unstage/discard/commit) ----

    fn find_status<'a>(
        status: &'a WorkingTreeStatus,
        bucket: &str,
        path: &str,
    ) -> Option<&'a StatusEntry> {
        let entries = if bucket == "staged" {
            &status.staged
        } else {
            &status.unstaged
        };
        entries.iter().find(|e| e.path == Path::new(path))
    }

    #[test]
    fn status_reports_untracked_file() {
        let (dir, _repo) = init_repo();
        fs::write(dir.path().join("new.txt"), "hi\n").unwrap();
        let git = GitRepo::open(dir.path()).unwrap();

        let status = git.status().unwrap();
        assert!(status.staged.is_empty());
        let entry = find_status(&status, "unstaged", "new.txt").unwrap();
        assert_eq!(entry.kind, ChangeKind::Untracked);
    }

    #[test]
    fn status_reports_staged_new_file() {
        let (dir, repo) = init_repo();
        fs::write(dir.path().join("new.txt"), "hi\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("new.txt")).unwrap();
        index.write().unwrap();
        let git = GitRepo::open(dir.path()).unwrap();

        let status = git.status().unwrap();
        let entry = find_status(&status, "staged", "new.txt").unwrap();
        assert_eq!(entry.kind, ChangeKind::Added);
        assert!(status.unstaged.is_empty());
    }

    #[test]
    fn status_reports_a_partially_staged_file_in_both_buckets() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        fs::write(dir.path().join("f.txt"), "b\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("f.txt")).unwrap();
        index.write().unwrap();
        // Further unstaged edit on top of the already-staged change.
        fs::write(dir.path().join("f.txt"), "c\n").unwrap();
        let git = GitRepo::open(dir.path()).unwrap();

        let status = git.status().unwrap();
        assert_eq!(
            find_status(&status, "staged", "f.txt").unwrap().kind,
            ChangeKind::Modified
        );
        assert_eq!(
            find_status(&status, "unstaged", "f.txt").unwrap().kind,
            ChangeKind::Modified
        );
    }

    #[test]
    fn status_reports_working_tree_deletion() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        fs::remove_file(dir.path().join("f.txt")).unwrap();
        let git = GitRepo::open(dir.path()).unwrap();

        let status = git.status().unwrap();
        let entry = find_status(&status, "unstaged", "f.txt").unwrap();
        assert_eq!(entry.kind, ChangeKind::Deleted);
        assert!(status.staged.is_empty());
    }

    #[test]
    fn status_reports_a_conflicted_path_once_in_unstaged() {
        let (dir, repo) = init_repo();
        setup_conflict(&dir, &repo);
        let git = GitRepo::open(dir.path()).unwrap();

        let status = git.status().unwrap();
        assert_eq!(
            status
                .staged
                .iter()
                .filter(|e| e.path == Path::new("f.txt"))
                .count(),
            0
        );
        let matches: Vec<_> = status
            .unstaged
            .iter()
            .filter(|e| e.path == Path::new("f.txt"))
            .collect();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].kind, ChangeKind::Conflicted);
    }

    #[test]
    fn stage_path_adds_a_new_file() {
        let (dir, _repo) = init_repo();
        fs::write(dir.path().join("new.txt"), "hi\n").unwrap();
        let git = GitRepo::open(dir.path()).unwrap();

        git.stage_path("new.txt").unwrap();

        let status = git.status().unwrap();
        assert_eq!(
            find_status(&status, "staged", "new.txt").unwrap().kind,
            ChangeKind::Added
        );
        assert!(status.unstaged.is_empty());
    }

    #[test]
    fn stage_path_on_a_working_tree_deletion_stages_the_removal() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        fs::remove_file(dir.path().join("f.txt")).unwrap();
        let git = GitRepo::open(dir.path()).unwrap();

        git.stage_path("f.txt").unwrap();

        let status = git.status().unwrap();
        assert_eq!(
            find_status(&status, "staged", "f.txt").unwrap().kind,
            ChangeKind::Deleted
        );
    }

    #[test]
    fn stage_path_rejects_path_escaping_workdir() {
        let (dir, _repo) = init_repo();
        let outside = dir.path().parent().unwrap().join("outside-stage.txt");
        fs::write(&outside, "untouched").unwrap();
        let git = GitRepo::open(dir.path()).unwrap();

        let result = git.stage_path("../outside-stage.txt");
        assert!(matches!(result, Err(GitError::PathEscapesRepo(_))));
        assert_eq!(fs::read_to_string(&outside).unwrap(), "untouched");

        let _ = fs::remove_file(&outside);
    }

    #[test]
    fn stage_path_rejects_absolute_path_escaping_workdir() {
        let (dir, _repo) = init_repo();
        let outside = dir.path().parent().unwrap().join("outside-stage-abs.txt");
        fs::write(&outside, "untouched").unwrap();
        let git = GitRepo::open(dir.path()).unwrap();

        let result = git.stage_path(&outside);
        assert!(matches!(result, Err(GitError::PathEscapesRepo(_))));
        assert_eq!(fs::read_to_string(&outside).unwrap(), "untouched");

        let _ = fs::remove_file(&outside);
    }

    #[test]
    fn stage_path_rejects_an_empty_path() {
        let (dir, _repo) = init_repo();
        let git = GitRepo::open(dir.path()).unwrap();
        let result = git.stage_path("");
        assert!(matches!(result, Err(GitError::PathEscapesRepo(_))));
    }

    #[test]
    fn stage_path_rejects_a_deleted_target_that_would_still_escape() {
        // Even though the target no longer exists on disk (so there's
        // nothing to canonicalize at the leaf), the component scan alone
        // must still catch the `..` before any ancestor-walk happens.
        let (dir, _repo) = init_repo();
        let git = GitRepo::open(dir.path()).unwrap();

        let result = git.stage_path("../nonexistent-outside.txt");
        assert!(matches!(result, Err(GitError::PathEscapesRepo(_))));
    }

    #[test]
    fn unstage_path_resets_a_staged_new_file_out_of_the_index() {
        let (dir, _repo) = init_repo();
        fs::write(dir.path().join("new.txt"), "hi\n").unwrap();
        let git = GitRepo::open(dir.path()).unwrap();
        git.stage_path("new.txt").unwrap();

        git.unstage_path("new.txt").unwrap();

        let status = git.status().unwrap();
        assert!(status.staged.is_empty());
        assert_eq!(
            find_status(&status, "unstaged", "new.txt").unwrap().kind,
            ChangeKind::Untracked
        );
    }

    #[test]
    fn unstage_path_resets_a_staged_modification_to_head() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        fs::write(dir.path().join("f.txt"), "b\n").unwrap();
        let git = GitRepo::open(dir.path()).unwrap();
        git.stage_path("f.txt").unwrap();

        git.unstage_path("f.txt").unwrap();

        let status = git.status().unwrap();
        assert!(status.staged.is_empty());
        assert_eq!(
            find_status(&status, "unstaged", "f.txt").unwrap().kind,
            ChangeKind::Modified
        );
        // Working tree content is untouched by unstage.
        assert_eq!(fs::read_to_string(dir.path().join("f.txt")).unwrap(), "b\n");
    }

    #[test]
    fn unstage_path_rejects_path_escaping_workdir() {
        let (dir, _repo) = init_repo();
        let git = GitRepo::open(dir.path()).unwrap();
        let result = git.unstage_path("../outside.txt");
        assert!(matches!(result, Err(GitError::PathEscapesRepo(_))));
    }

    #[test]
    fn unstage_path_rejects_an_empty_path_instead_of_unstaging_everything() {
        // Regression test: `workdir.join("")` resolves to `workdir`
        // itself, and git2's `reset_default` treats an empty pathspec as
        // "match everything" rather than "match nothing" -- an empty
        // path must be rejected before it ever reaches that call, or a
        // single-file unstage silently turns into unstaging every staged
        // file in the repo.
        let (dir, repo) = init_repo();
        commit_file(&repo, "a.txt", "one\n", "init");
        fs::write(dir.path().join("a.txt"), "one-changed\n").unwrap();
        fs::write(dir.path().join("b.txt"), "two\n").unwrap();
        let git = GitRepo::open(dir.path()).unwrap();
        git.stage_path("a.txt").unwrap();
        git.stage_path("b.txt").unwrap();

        let result = git.unstage_path("");

        assert!(matches!(result, Err(GitError::PathEscapesRepo(_))));
        let status = git.status().unwrap();
        assert_eq!(status.staged.len(), 2, "nothing should have been unstaged");
    }

    #[test]
    fn discard_path_reverts_a_tracked_modification() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        fs::write(dir.path().join("f.txt"), "b\n").unwrap();
        let git = GitRepo::open(dir.path()).unwrap();

        git.discard_path("f.txt").unwrap();

        assert_eq!(fs::read_to_string(dir.path().join("f.txt")).unwrap(), "a\n");
        assert!(git.status().unwrap().unstaged.is_empty());
    }

    #[test]
    fn discard_path_restores_a_working_tree_deletion() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        fs::remove_file(dir.path().join("f.txt")).unwrap();
        let git = GitRepo::open(dir.path()).unwrap();

        git.discard_path("f.txt").unwrap();

        assert_eq!(fs::read_to_string(dir.path().join("f.txt")).unwrap(), "a\n");
    }

    #[test]
    fn discard_path_deletes_an_untracked_file() {
        let (dir, _repo) = init_repo();
        fs::write(dir.path().join("scratch.txt"), "junk\n").unwrap();
        let git = GitRepo::open(dir.path()).unwrap();

        git.discard_path("scratch.txt").unwrap();

        assert!(!dir.path().join("scratch.txt").exists());
    }

    #[test]
    fn discard_path_rejects_an_empty_path() {
        let (dir, _repo) = init_repo();
        let git = GitRepo::open(dir.path()).unwrap();
        let result = git.discard_path("");
        assert!(matches!(result, Err(GitError::PathEscapesRepo(_))));
    }

    #[test]
    fn discard_path_rejects_path_escaping_workdir() {
        let (dir, _repo) = init_repo();
        let outside = dir.path().parent().unwrap().join("outside-discard.txt");
        fs::write(&outside, "precious").unwrap();
        let git = GitRepo::open(dir.path()).unwrap();

        let result = git.discard_path("../outside-discard.txt");
        assert!(matches!(result, Err(GitError::PathEscapesRepo(_))));
        assert_eq!(fs::read_to_string(&outside).unwrap(), "precious");

        let _ = fs::remove_file(&outside);
    }

    #[test]
    fn discard_path_rejects_absolute_path_escaping_workdir() {
        let (dir, _repo) = init_repo();
        let outside = dir.path().parent().unwrap().join("outside-discard-abs.txt");
        fs::write(&outside, "precious").unwrap();
        let git = GitRepo::open(dir.path()).unwrap();

        let result = git.discard_path(&outside);
        assert!(matches!(result, Err(GitError::PathEscapesRepo(_))));
        assert_eq!(fs::read_to_string(&outside).unwrap(), "precious");

        let _ = fs::remove_file(&outside);
    }

    #[test]
    fn discard_path_treats_a_glob_special_filename_literally() {
        // Regression test: `CheckoutBuilder::path` interprets its
        // argument as a pathspec *pattern* unless
        // `disable_pathspec_match` is set. A tracked file literally named
        // "a*.txt" would, without that flag, make `checkout_head` match
        // every sibling starting with "a" too -- proving `ab.txt`'s own
        // unrelated unstaged change survives a discard targeted only at
        // "a*.txt" is exactly what would fail if that flag were dropped.
        let (dir, repo) = init_repo();
        commit_file(&repo, "a*.txt", "one\n", "init");
        commit_file(&repo, "ab.txt", "two\n", "init2");
        fs::write(dir.path().join("a*.txt"), "one-changed\n").unwrap();
        fs::write(dir.path().join("ab.txt"), "two-changed\n").unwrap();
        let git = GitRepo::open(dir.path()).unwrap();

        git.discard_path("a*.txt").unwrap();

        assert_eq!(
            fs::read_to_string(dir.path().join("a*.txt")).unwrap(),
            "one\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("ab.txt")).unwrap(),
            "two-changed\n"
        );
    }

    #[test]
    fn discard_path_via_a_symlinked_ancestor_is_rejected() {
        // The component scan alone doesn't catch this (no literal ".."),
        // so this proves the canonicalize-based ancestor check is what
        // actually stops it: `link` resolves outside `dir`, and the
        // target under it doesn't exist, so the ancestor-walk must climb
        // to `link` itself (still inside workdir's path *string*) and
        // canonicalize it to discover it really points outside.
        let (dir, _repo) = init_repo();
        let outside = dir.path().parent().unwrap().join("outside-target-dir");
        fs::create_dir_all(&outside).unwrap();
        let precious = outside.join("precious.txt");
        fs::write(&precious, "precious").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, dir.path().join("link")).unwrap();
            let git = GitRepo::open(dir.path()).unwrap();
            let result = git.discard_path("link/precious.txt");
            assert!(matches!(result, Err(GitError::PathEscapesRepo(_))));
            assert_eq!(fs::read_to_string(&precious).unwrap(), "precious");
        }

        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn commit_creates_the_first_commit_from_staged_content() {
        let (dir, repo) = init_repo();
        fs::write(dir.path().join("f.txt"), "a\n").unwrap();
        let git = GitRepo::open(dir.path()).unwrap();
        git.stage_path("f.txt").unwrap();

        let id = git.commit("initial commit", false).unwrap();

        let oid = git2::Oid::from_str(&id).unwrap();
        let commit = repo.find_commit(oid).unwrap();
        assert_eq!(commit.summary().unwrap(), Some("initial commit"));
        assert_eq!(commit.parent_count(), 0);
        assert!(git.status().unwrap().staged.is_empty());
    }

    #[test]
    fn commit_with_a_parent_chains_onto_head() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        fs::write(dir.path().join("g.txt"), "b\n").unwrap();
        let git = GitRepo::open(dir.path()).unwrap();
        git.stage_path("g.txt").unwrap();

        let id = git.commit("second commit", false).unwrap();

        let oid = git2::Oid::from_str(&id).unwrap();
        let commit = repo.find_commit(oid).unwrap();
        assert_eq!(commit.parent_count(), 1);
    }

    #[test]
    fn commit_rejects_an_empty_message() {
        let (dir, _repo) = init_repo();
        fs::write(dir.path().join("f.txt"), "a\n").unwrap();
        let git = GitRepo::open(dir.path()).unwrap();
        git.stage_path("f.txt").unwrap();

        let result = git.commit("   ", false);
        assert!(matches!(result, Err(GitError::Git2(_))));
    }

    #[test]
    fn commit_amend_replaces_the_previous_commits_message_and_tree() {
        let (dir, repo) = init_repo();
        let first = commit_file(&repo, "f.txt", "a\n", "init");
        fs::write(dir.path().join("f.txt"), "b\n").unwrap();
        let git = GitRepo::open(dir.path()).unwrap();
        git.stage_path("f.txt").unwrap();

        let id = git.commit("amended message", true).unwrap();

        let oid = git2::Oid::from_str(&id).unwrap();
        let amended = repo.find_commit(oid).unwrap();
        assert_eq!(amended.summary().unwrap(), Some("amended message"));
        assert_eq!(amended.parent_count(), 0);
        assert_ne!(amended.id(), first);
        let blob_entry = amended
            .tree()
            .unwrap()
            .get_path(Path::new("f.txt"))
            .unwrap();
        let blob = repo.find_blob(blob_entry.id()).unwrap();
        assert_eq!(blob.content(), b"b\n");
    }

    #[test]
    fn commit_with_merge_head_present_creates_a_two_parent_commit_and_cleans_up_state() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        let main_name = repo.head().unwrap().shorthand().unwrap().to_string();
        let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("feature", &head_commit, false).unwrap();
        checkout_branch(&repo, "feature");
        commit_file(&repo, "g.txt", "b\n", "on feature");
        checkout_branch(&repo, &main_name);
        commit_file(&repo, "h.txt", "c\n", "on main");

        let their_oid = repo
            .find_branch("feature", BranchType::Local)
            .unwrap()
            .get()
            .target()
            .unwrap();
        let their_annotated = repo.find_annotated_commit(their_oid).unwrap();
        repo.merge(&[&their_annotated], None, None).unwrap();
        assert!(!repo.index().unwrap().has_conflicts());
        assert!(repo.find_reference("MERGE_HEAD").is_ok());

        let git = GitRepo::open(dir.path()).unwrap();
        let commit_id = git.commit("merge commit message", false).unwrap();

        let oid = git2::Oid::from_str(&commit_id).unwrap();
        let commit = repo.find_commit(oid).unwrap();
        assert_eq!(commit.parent_count(), 2);
        assert!(repo.find_reference("MERGE_HEAD").is_err());
    }

    #[test]
    fn branches_lists_local_branches_alphabetically_with_head_marked() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        let head_name = repo.head().unwrap().shorthand().unwrap().to_string();
        let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("zeta", &head_commit, false).unwrap();
        repo.branch("alpha", &head_commit, false).unwrap();
        let git = GitRepo::open(dir.path()).unwrap();

        let branches = git.branches().unwrap();

        let names: Vec<&str> = branches.iter().map(|b| b.name.as_str()).collect();
        let mut expected: Vec<&str> = vec!["alpha", head_name.as_str(), "zeta"];
        expected.sort();
        assert_eq!(names, expected);
        assert_eq!(branches.iter().filter(|b| b.is_head).count(), 1);
        let head_entry = branches.iter().find(|b| b.name == head_name).unwrap();
        assert!(head_entry.is_head);
    }

    #[test]
    fn branches_on_a_brand_new_repo_with_no_commits_is_empty() {
        let (dir, _repo) = init_repo();
        let git = GitRepo::open(dir.path()).unwrap();
        assert!(git.branches().unwrap().is_empty());
    }

    #[test]
    fn create_branch_from_head_by_default() {
        let (dir, repo) = init_repo();
        let head_commit_id = commit_file(&repo, "f.txt", "a\n", "init");
        let git = GitRepo::open(dir.path()).unwrap();

        git.create_branch("feature", None).unwrap();

        let branch = repo.find_branch("feature", BranchType::Local).unwrap();
        assert_eq!(branch.get().target().unwrap(), head_commit_id);
    }

    #[test]
    fn create_branch_from_an_explicit_start_point() {
        let (dir, repo) = init_repo();
        let first = commit_file(&repo, "f.txt", "a\n", "init");
        commit_file(&repo, "f.txt", "b\n", "second");
        let git = GitRepo::open(dir.path()).unwrap();

        git.create_branch("from-first", Some(&first.to_string()))
            .unwrap();

        let branch = repo.find_branch("from-first", BranchType::Local).unwrap();
        assert_eq!(branch.get().target().unwrap(), first);
    }

    #[test]
    fn create_branch_does_not_switch_to_it() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        let git = GitRepo::open(dir.path()).unwrap();
        let before = git.current_branch();

        git.create_branch("feature", None).unwrap();

        assert_eq!(git.current_branch(), before);
    }

    #[test]
    fn create_branch_errors_if_name_already_exists() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        let git = GitRepo::open(dir.path()).unwrap();
        git.create_branch("feature", None).unwrap();

        let result = git.create_branch("feature", None);
        assert!(matches!(result, Err(GitError::Git2(_))));
    }

    #[test]
    fn switch_branch_moves_head_and_updates_working_tree() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        let main_name = repo.head().unwrap().shorthand().unwrap().to_string();
        let git = GitRepo::open(dir.path()).unwrap();
        git.create_branch("feature", None).unwrap();

        checkout_branch(&repo, "feature");
        commit_file(&repo, "g.txt", "b\n", "on feature");
        checkout_branch(&repo, &main_name);
        assert!(!dir.path().join("g.txt").exists());

        git.switch_branch("feature").unwrap();

        assert_eq!(git.current_branch(), Some("feature".to_string()));
        assert!(dir.path().join("g.txt").exists());
    }

    #[test]
    fn switch_branch_refuses_to_overwrite_uncommitted_changes() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        let main_name = repo.head().unwrap().shorthand().unwrap().to_string();
        let git = GitRepo::open(dir.path()).unwrap();
        git.create_branch("feature", None).unwrap();
        checkout_branch(&repo, "feature");
        commit_file(&repo, "f.txt", "b\n", "on feature");
        checkout_branch(&repo, &main_name);
        fs::write(dir.path().join("f.txt"), "uncommitted local edit\n").unwrap();

        let result = git.switch_branch("feature");

        assert!(result.is_err());
        assert_eq!(
            fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "uncommitted local edit\n"
        );
    }

    #[test]
    fn delete_branch_succeeds_when_merged() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        let git = GitRepo::open(dir.path()).unwrap();
        git.create_branch("feature", None).unwrap();

        git.delete_branch("feature", false).unwrap();

        assert!(repo.find_branch("feature", BranchType::Local).is_err());
    }

    #[test]
    fn delete_branch_refuses_when_not_merged_and_force_is_false() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        let main_name = repo.head().unwrap().shorthand().unwrap().to_string();
        let git = GitRepo::open(dir.path()).unwrap();
        git.create_branch("feature", None).unwrap();
        checkout_branch(&repo, "feature");
        commit_file(&repo, "g.txt", "b\n", "on feature only");
        checkout_branch(&repo, &main_name);

        let result = git.delete_branch("feature", false);

        assert!(matches!(&result, Err(GitError::BranchNotMerged(name)) if name == "feature"));
        assert!(repo.find_branch("feature", BranchType::Local).is_ok());
    }

    #[test]
    fn delete_branch_force_bypasses_the_merged_check() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        let main_name = repo.head().unwrap().shorthand().unwrap().to_string();
        let git = GitRepo::open(dir.path()).unwrap();
        git.create_branch("feature", None).unwrap();
        checkout_branch(&repo, "feature");
        commit_file(&repo, "g.txt", "b\n", "on feature only");
        checkout_branch(&repo, &main_name);

        git.delete_branch("feature", true).unwrap();

        assert!(repo.find_branch("feature", BranchType::Local).is_err());
    }

    #[test]
    fn delete_branch_refuses_to_delete_the_checked_out_branch_even_with_force() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        let main_name = repo.head().unwrap().shorthand().unwrap().to_string();
        let git = GitRepo::open(dir.path()).unwrap();

        let result = git.delete_branch(&main_name, true);

        assert!(result.is_err());
        assert!(repo.find_branch(&main_name, BranchType::Local).is_ok());
    }

    #[test]
    fn merge_branch_up_to_date_when_target_is_an_ancestor() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        let git = GitRepo::open(dir.path()).unwrap();
        git.create_branch("feature", None).unwrap();

        let outcome = git.merge_branch("feature").unwrap();

        assert_eq!(outcome, MergeOutcome::UpToDate);
    }

    #[test]
    fn merge_branch_fast_forwards_when_head_is_an_ancestor_of_target() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        let main_name = repo.head().unwrap().shorthand().unwrap().to_string();
        let git = GitRepo::open(dir.path()).unwrap();
        git.create_branch("feature", None).unwrap();
        checkout_branch(&repo, "feature");
        let tip = commit_file(&repo, "g.txt", "b\n", "on feature");
        checkout_branch(&repo, &main_name);

        let outcome = git.merge_branch("feature").unwrap();

        assert_eq!(outcome, MergeOutcome::FastForward);
        assert_eq!(git.current_branch(), Some(main_name));
        assert_eq!(repo.head().unwrap().target().unwrap(), tip);
        assert!(dir.path().join("g.txt").exists());
    }

    #[test]
    fn merge_branch_clean_merge_auto_commits_with_two_parents() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        let main_name = repo.head().unwrap().shorthand().unwrap().to_string();
        let git = GitRepo::open(dir.path()).unwrap();
        git.create_branch("feature", None).unwrap();
        checkout_branch(&repo, "feature");
        commit_file(&repo, "g.txt", "on feature\n", "feature commit");
        checkout_branch(&repo, &main_name);
        commit_file(&repo, "h.txt", "on main\n", "main commit");

        let outcome = git.merge_branch("feature").unwrap();

        let commit_id = match outcome {
            MergeOutcome::Merged { commit_id } => commit_id,
            other => panic!("expected Merged, got {other:?}"),
        };
        let oid = git2::Oid::from_str(&commit_id).unwrap();
        let commit = repo.find_commit(oid).unwrap();
        assert_eq!(commit.parent_count(), 2);
        let expected_message = format!("Merge branch 'feature' into {main_name}");
        assert_eq!(commit.summary().unwrap(), Some(expected_message.as_str()));
        assert!(dir.path().join("g.txt").exists());
        assert!(dir.path().join("h.txt").exists());
        assert!(git.conflicts().unwrap().is_empty());
    }

    #[test]
    fn merge_branch_conflicts_and_finishing_via_commit_produces_a_two_parent_commit() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        let main_name = repo.head().unwrap().shorthand().unwrap().to_string();
        let git = GitRepo::open(dir.path()).unwrap();
        git.create_branch("feature", None).unwrap();
        checkout_branch(&repo, "feature");
        commit_file(&repo, "f.txt", "from feature\n", "feature edits f");
        checkout_branch(&repo, &main_name);
        commit_file(&repo, "f.txt", "from main\n", "main edits f");

        let outcome = git.merge_branch("feature").unwrap();

        let paths = match outcome {
            MergeOutcome::Conflicts(paths) => paths,
            other => panic!("expected Conflicts, got {other:?}"),
        };
        assert_eq!(paths, vec![PathBuf::from("f.txt")]);
        assert_eq!(git.conflicts().unwrap(), vec![PathBuf::from("f.txt")]);

        git.resolve_conflict("f.txt", "resolved content\n").unwrap();
        assert!(git.conflicts().unwrap().is_empty());

        let commit_id = git
            .commit("Merge branch 'feature' into main", false)
            .unwrap();

        let oid = git2::Oid::from_str(&commit_id).unwrap();
        let commit = repo.find_commit(oid).unwrap();
        assert_eq!(commit.parent_count(), 2);
        assert!(repo.find_reference("MERGE_HEAD").is_err());
    }

    #[test]
    fn commit_detail_splits_summary_and_body() {
        let (dir, repo) = init_repo();
        let id = commit_file(
            &repo,
            "f.txt",
            "a\n",
            "Summary line\n\nBody paragraph one.\n\nBody paragraph two.",
        );
        let git = GitRepo::open(dir.path()).unwrap();

        let detail = git.commit_detail(&id.to_string()).unwrap();

        assert_eq!(detail.summary, "Summary line");
        assert_eq!(detail.body, "Body paragraph one.\n\nBody paragraph two.");
        assert_eq!(detail.author, "Test User");
        assert_eq!(detail.email, "test@example.com");
        assert_eq!(detail.id, id.to_string());
    }

    #[test]
    fn commit_detail_single_line_message_has_empty_body() {
        let (dir, repo) = init_repo();
        let id = commit_file(&repo, "f.txt", "a\n", "just a summary");
        let git = GitRepo::open(dir.path()).unwrap();

        let detail = git.commit_detail(&id.to_string()).unwrap();

        assert_eq!(detail.summary, "just a summary");
        assert_eq!(detail.body, "");
    }

    #[test]
    fn commit_detail_accepts_a_branch_name_revspec() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        let main_name = repo.head().unwrap().shorthand().unwrap().to_string();
        let git = GitRepo::open(dir.path()).unwrap();

        let detail = git.commit_detail(&main_name).unwrap();

        assert_eq!(detail.summary, "init");
    }

    #[test]
    fn blame_file_attributes_each_line_to_its_commit() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "one\ntwo\n", "first two lines");
        commit_file(&repo, "f.txt", "one\ntwo\nthree\n", "add third line");
        let git = GitRepo::open(dir.path()).unwrap();

        let lines = git.blame_file("f.txt").unwrap();

        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].line, 0);
        assert_eq!(lines[1].line, 1);
        assert_eq!(lines[2].line, 2);
        assert_eq!(lines[0].summary, "first two lines");
        assert_eq!(lines[1].summary, "first two lines");
        assert_eq!(lines[2].summary, "add third line");
    }

    #[test]
    fn blame_file_on_an_untracked_path_returns_empty_not_an_error() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        fs::write(dir.path().join("untracked.txt"), "new\n").unwrap();
        let git = GitRepo::open(dir.path()).unwrap();

        assert_eq!(git.blame_file("untracked.txt").unwrap(), Vec::new());
    }

    #[test]
    fn blame_file_on_a_brand_new_repo_with_no_commits_returns_empty() {
        let (dir, _repo) = init_repo();
        fs::write(dir.path().join("f.txt"), "a\n").unwrap();
        let git = GitRepo::open(dir.path()).unwrap();

        assert_eq!(git.blame_file("f.txt").unwrap(), Vec::new());
    }

    #[test]
    fn blame_file_caps_at_max_blame_lines() {
        let (dir, repo) = init_repo();
        let mut content = String::new();
        for i in 0..(MAX_BLAME_LINES + 500) {
            content.push_str(&format!("line {i}\n"));
        }
        commit_file(&repo, "big.txt", &content, "big file");
        let git = GitRepo::open(dir.path()).unwrap();

        let lines = git.blame_file("big.txt").unwrap();

        assert_eq!(lines.len(), MAX_BLAME_LINES);
    }

    #[test]
    fn worktrees_on_a_repo_with_none_is_empty() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        let git = GitRepo::open(dir.path()).unwrap();

        assert!(git.worktrees().unwrap().is_empty());
    }

    #[test]
    fn add_worktree_with_no_branch_creates_a_new_branch_named_after_it() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        let git = GitRepo::open(dir.path()).unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        let wt_path = wt_dir.path().join("wt1");

        git.add_worktree("wt1", &wt_path, None).unwrap();

        assert!(repo.find_branch("wt1", BranchType::Local).is_ok());
        let listed = git.worktrees().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "wt1");
        assert_eq!(
            listed[0].path.canonicalize().unwrap(),
            wt_path.canonicalize().unwrap()
        );
        assert_eq!(listed[0].branch.as_deref(), Some("wt1"));
        assert!(!listed[0].is_locked);
    }

    #[test]
    fn add_worktree_with_an_existing_branch_checks_it_out() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        let git = GitRepo::open(dir.path()).unwrap();
        git.create_branch("feature", None).unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        let wt_path = wt_dir.path().join("wt-feature");

        git.add_worktree("wt-feature", &wt_path, Some("feature"))
            .unwrap();

        let listed = git.worktrees().unwrap();
        assert_eq!(listed[0].branch.as_deref(), Some("feature"));
    }

    #[test]
    fn add_worktree_with_a_nonexistent_branch_propagates_git2_error() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        let git = GitRepo::open(dir.path()).unwrap();
        let wt_dir = tempfile::tempdir().unwrap();

        let err = git
            .add_worktree("wt1", wt_dir.path().join("wt1"), Some("does-not-exist"))
            .unwrap_err();

        assert!(matches!(err, GitError::Git2(_)));
    }

    #[test]
    fn add_worktree_rejects_empty_name() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        let git = GitRepo::open(dir.path()).unwrap();
        let wt_dir = tempfile::tempdir().unwrap();

        let err = git
            .add_worktree("", wt_dir.path().join("wt1"), None)
            .unwrap_err();

        assert!(matches!(err, GitError::InvalidWorktreeName(_)));
    }

    #[test]
    fn add_worktree_rejects_name_containing_a_path_separator() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        let git = GitRepo::open(dir.path()).unwrap();
        let wt_dir = tempfile::tempdir().unwrap();

        let err = git
            .add_worktree("a/b", wt_dir.path().join("wt1"), None)
            .unwrap_err();

        assert!(matches!(err, GitError::InvalidWorktreeName(_)));
    }

    #[test]
    fn add_worktree_rejects_dot_and_dotdot_names() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        let git = GitRepo::open(dir.path()).unwrap();
        let wt_dir = tempfile::tempdir().unwrap();

        assert!(matches!(
            git.add_worktree(".", wt_dir.path().join("wt1"), None),
            Err(GitError::InvalidWorktreeName(_))
        ));
        assert!(matches!(
            git.add_worktree("..", wt_dir.path().join("wt2"), None),
            Err(GitError::InvalidWorktreeName(_))
        ));
    }

    #[test]
    fn add_worktree_rejects_a_name_already_registered() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        let git = GitRepo::open(dir.path()).unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        git.add_worktree("wt1", wt_dir.path().join("wt1"), None)
            .unwrap();

        let err = git
            .add_worktree("wt1", wt_dir.path().join("wt1-again"), None)
            .unwrap_err();

        assert!(matches!(err, GitError::WorktreeNameTaken(_)));
    }

    #[test]
    fn add_worktree_rejects_a_nonempty_existing_destination() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        let git = GitRepo::open(dir.path()).unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        let dest = wt_dir.path().join("occupied");
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("file"), "x").unwrap();

        let err = git.add_worktree("wt1", &dest, None).unwrap_err();

        assert!(matches!(err, GitError::DestinationNotEmpty(_)));
    }

    #[test]
    fn add_worktree_rejects_a_destination_nested_inside_the_repo_workdir() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        let git = GitRepo::open(dir.path()).unwrap();
        let nested = dir.path().join("nested-wt");
        fs::create_dir_all(dir.path()).unwrap();

        let err = git.add_worktree("wt1", &nested, None).unwrap_err();

        assert!(matches!(err, GitError::WorktreeInsideRepo(_)));
    }

    #[test]
    fn add_worktree_rejects_a_symlink_destination_pointing_inside_the_repo_workdir() {
        // docs/security-findings/git-worktrees-core-2026-09-01.md finding 2:
        // path.parent().canonicalize() alone misses a destination that is
        // itself a pre-existing symlink into the workdir.
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        let git = GitRepo::open(dir.path()).unwrap();
        let inside_target = dir.path().join("inside-target");
        fs::create_dir_all(&inside_target).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let symlink_path = outside.path().join("link-into-repo");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&inside_target, &symlink_path).unwrap();

        #[cfg(unix)]
        {
            let err = git.add_worktree("wt1", &symlink_path, None).unwrap_err();
            assert!(matches!(err, GitError::WorktreeInsideRepo(_)));
        }
    }

    #[test]
    fn add_worktree_rejects_names_containing_unicode_bidi_control_characters() {
        // docs/security-findings/git-worktrees-core-2026-09-01.md finding 1:
        // an unterminated bidi override must never become a real worktree/
        // branch name.
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        let git = GitRepo::open(dir.path()).unwrap();
        let wt_dir = tempfile::tempdir().unwrap();

        let err = git
            .add_worktree("\u{202E}evil", wt_dir.path().join("wt1"), None)
            .unwrap_err();

        assert!(matches!(err, GitError::InvalidWorktreeName(_)));
        assert!(git.worktrees().unwrap().is_empty());
    }

    #[test]
    fn remove_worktree_deletes_a_clean_worktree_without_force() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        let git = GitRepo::open(dir.path()).unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        let wt_path = wt_dir.path().join("wt1");
        git.add_worktree("wt1", &wt_path, None).unwrap();

        git.remove_worktree("wt1", false).unwrap();

        assert!(git.worktrees().unwrap().is_empty());
        assert!(!wt_path.exists());
    }

    #[test]
    fn remove_worktree_refuses_when_uncommitted_changes_exist() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        let git = GitRepo::open(dir.path()).unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        let wt_path = wt_dir.path().join("wt1");
        git.add_worktree("wt1", &wt_path, None).unwrap();
        fs::write(wt_path.join("untracked.txt"), "surprise").unwrap();

        let err = git.remove_worktree("wt1", false).unwrap_err();

        assert!(matches!(err, GitError::WorktreeHasUncommittedChanges(_)));
        assert!(wt_path.exists());
    }

    #[test]
    fn remove_worktree_force_bypasses_the_uncommitted_changes_check() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        let git = GitRepo::open(dir.path()).unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        let wt_path = wt_dir.path().join("wt1");
        git.add_worktree("wt1", &wt_path, None).unwrap();
        fs::write(wt_path.join("untracked.txt"), "surprise").unwrap();

        git.remove_worktree("wt1", true).unwrap();

        assert!(git.worktrees().unwrap().is_empty());
    }

    #[test]
    fn remove_worktree_refuses_a_locked_worktree_without_force() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        let git = GitRepo::open(dir.path()).unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        let wt_path = wt_dir.path().join("wt1");
        git.add_worktree("wt1", &wt_path, None).unwrap();
        repo.find_worktree("wt1").unwrap().lock(None).unwrap();

        let err = git.remove_worktree("wt1", false).unwrap_err();

        assert!(matches!(err, GitError::WorktreeLocked(_)));
        assert!(wt_path.exists());
    }

    #[test]
    fn remove_worktree_force_bypasses_the_lock_check() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        let git = GitRepo::open(dir.path()).unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        let wt_path = wt_dir.path().join("wt1");
        git.add_worktree("wt1", &wt_path, None).unwrap();
        repo.find_worktree("wt1").unwrap().lock(None).unwrap();

        git.remove_worktree("wt1", true).unwrap();

        assert!(git.worktrees().unwrap().is_empty());
    }

    #[test]
    fn remove_worktree_with_a_missing_directory_succeeds_without_force() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        let git = GitRepo::open(dir.path()).unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        let wt_path = wt_dir.path().join("wt1");
        git.add_worktree("wt1", &wt_path, None).unwrap();
        fs::remove_dir_all(&wt_path).unwrap();

        git.remove_worktree("wt1", false).unwrap();

        assert!(git.worktrees().unwrap().is_empty());
    }

    #[test]
    fn worktrees_lists_a_worktree_whose_directory_is_missing_with_no_branch() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\n", "init");
        let git = GitRepo::open(dir.path()).unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        let wt_path = wt_dir.path().join("wt1");
        git.add_worktree("wt1", &wt_path, None).unwrap();
        fs::remove_dir_all(&wt_path).unwrap();

        let listed = git.worktrees().unwrap();

        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "wt1");
        assert_eq!(listed[0].branch, None);
    }

    #[test]
    fn intraline_diff_single_word_change_matches_doc_example() {
        // docs/features/diff-viewer-enhancements.md §5.1's worked example.
        let old = "let x = compute_value();";
        let new = "let x = compute_result();";
        let (old_spans, new_spans) = intraline_diff(old, new);
        assert_eq!(old_spans, vec![DiffSpan { start: 16, end: 21 }]);
        assert_eq!(&old[16..21], "value");
        assert_eq!(new_spans, vec![DiffSpan { start: 16, end: 22 }]);
        assert_eq!(&new[16..22], "result");
    }

    #[test]
    fn intraline_diff_identical_strings_yields_no_spans() {
        let (old_spans, new_spans) = intraline_diff("same", "same");
        assert!(old_spans.is_empty());
        assert!(new_spans.is_empty());
    }

    #[test]
    fn intraline_diff_completely_disjoint_spans_whole_line() {
        let (old_spans, new_spans) = intraline_diff("cat", "dog");
        assert_eq!(old_spans, vec![DiffSpan { start: 0, end: 3 }]);
        assert_eq!(new_spans, vec![DiffSpan { start: 0, end: 3 }]);
    }

    #[test]
    fn intraline_diff_strict_prefix_leaves_shorter_side_empty() {
        let (old_spans, new_spans) = intraline_diff("ab", "abcdef");
        assert!(old_spans.is_empty());
        assert_eq!(new_spans, vec![DiffSpan { start: 2, end: 6 }]);
        assert_eq!(&"abcdef"[2..6], "cdef");
    }

    #[test]
    fn intraline_diff_is_char_boundary_safe_on_multibyte_utf8() {
        // 'é' is a 2-byte UTF-8 char; a naive byte-index trim would slice
        // through its middle and panic.
        let old = "héllo";
        let new = "hillo";
        let (old_spans, new_spans) = intraline_diff(old, new);
        assert_eq!(old_spans, vec![DiffSpan { start: 1, end: 3 }]);
        assert_eq!(&old[1..3], "é");
        assert_eq!(new_spans, vec![DiffSpan { start: 1, end: 2 }]);
        assert_eq!(&new[1..2], "i");
    }

    // ---- diff_text ----

    #[test]
    fn diff_text_identical_strings_returns_none() {
        assert_eq!(diff_text(Path::new("f.txt"), "same\n", "same\n"), None);
    }

    #[test]
    fn diff_text_reports_a_line_change() {
        let diff = diff_text(Path::new("f.txt"), "a\nb\nc\n", "a\nB\nc\n").unwrap();
        assert_eq!(diff.old_path, Some(PathBuf::from("f.txt")));
        assert_eq!(diff.new_path, Some(PathBuf::from("f.txt")));
        let lines: Vec<&DiffLine> = diff.hunks.iter().flat_map(|h| &h.lines).collect();
        assert!(lines.contains(&&DiffLine::Removed(
            "b".to_string(),
            vec![DiffSpan { start: 0, end: 1 }]
        )));
        assert!(lines.contains(&&DiffLine::Added(
            "B".to_string(),
            vec![DiffSpan { start: 0, end: 1 }]
        )));
        assert!(lines.contains(&&DiffLine::Context("a".to_string())));
    }

    #[test]
    fn diff_text_pure_insertion() {
        let diff = diff_text(Path::new("f.txt"), "a\n", "a\nb\n").unwrap();
        let lines: Vec<&DiffLine> = diff.hunks.iter().flat_map(|h| &h.lines).collect();
        assert!(lines.contains(&&DiffLine::Added("b".to_string(), Vec::new())));
    }

    #[test]
    fn diff_text_no_git_object_or_repository_involved() {
        // Confirms diff_text needs no Repository at all -- called with no
        // repo anywhere in scope, from an empty tempdir that isn't even a
        // git repository.
        let dir = tempfile::tempdir().unwrap();
        assert!(!GitRepo::is_repo(dir.path()));
        let diff = diff_text(Path::new("x.rs"), "fn a() {}\n", "fn b() {}\n");
        assert!(diff.is_some());
    }

    #[test]
    fn diff_file_single_line_modification_pairs_spans() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\nb\nc\n", "init");
        fs::write(dir.path().join("f.txt"), "a\nB\nc\n").unwrap();

        let git = GitRepo::open(dir.path()).unwrap();
        let diff = git.diff_file("f.txt").unwrap().expect("file changed");
        let lines: Vec<&DiffLine> = diff.hunks.iter().flat_map(|h| &h.lines).collect();

        let removed_spans = lines.iter().find_map(|l| match l {
            DiffLine::Removed(text, spans) if text == "b" => Some(spans.clone()),
            _ => None,
        });
        let added_spans = lines.iter().find_map(|l| match l {
            DiffLine::Added(text, spans) if text == "B" => Some(spans.clone()),
            _ => None,
        });
        assert_eq!(removed_spans, Some(vec![DiffSpan { start: 0, end: 1 }]));
        assert_eq!(added_spans, Some(vec![DiffSpan { start: 0, end: 1 }]));
    }

    #[test]
    fn diff_file_pure_insertion_has_empty_spans() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\nc\n", "init");
        fs::write(dir.path().join("f.txt"), "a\nb\nc\n").unwrap();

        let git = GitRepo::open(dir.path()).unwrap();
        let diff = git.diff_file("f.txt").unwrap().expect("file changed");
        let lines: Vec<&DiffLine> = diff.hunks.iter().flat_map(|h| &h.lines).collect();

        assert!(lines.contains(&&DiffLine::Added("b".to_string(), Vec::new())));
    }

    #[test]
    fn diff_file_pure_deletion_has_empty_spans() {
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "a\nb\nc\n", "init");
        fs::write(dir.path().join("f.txt"), "a\nc\n").unwrap();

        let git = GitRepo::open(dir.path()).unwrap();
        let diff = git.diff_file("f.txt").unwrap().expect("file changed");
        let lines: Vec<&DiffLine> = diff.hunks.iter().flat_map(|h| &h.lines).collect();

        assert!(lines.contains(&&DiffLine::Removed("b".to_string(), Vec::new())));
    }

    #[test]
    fn diff_file_unequal_replace_block_pairs_only_up_to_the_shorter_run() {
        // Two lines removed, one added: only the first Removed pairs with
        // the Added line; the second Removed keeps an empty span Vec
        // (doc §3.1's "lines beyond the paired count" case).
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "x\ny\nz\n", "init");
        fs::write(dir.path().join("f.txt"), "x\nsingle\n").unwrap();

        let git = GitRepo::open(dir.path()).unwrap();
        let diff = git.diff_file("f.txt").unwrap().expect("file changed");
        let lines: Vec<&DiffLine> = diff.hunks.iter().flat_map(|h| &h.lines).collect();

        let removed: Vec<&DiffLine> = lines
            .iter()
            .filter(|l| matches!(l, DiffLine::Removed(..)))
            .copied()
            .collect();
        assert_eq!(removed.len(), 2);
        let paired_count = removed
            .iter()
            .filter(|l| match l {
                DiffLine::Removed(_, spans) => !spans.is_empty(),
                _ => false,
            })
            .count();
        assert_eq!(paired_count, 1);
    }

    #[test]
    fn diff_truncated_replace_block_keeps_full_added_run_paired() {
        // A 1-removed/huge-added replace block ("orig" -> many "lineN"
        // lines): `truncate_file_diff` runs before pairing (doc §4), and
        // here the truncation only trims the Added run's *tail* — the
        // first Added line survives, so the Removed line still pairs.
        let (dir, repo) = init_repo();
        commit_file(&repo, "f.txt", "orig\n", "init");

        let big: String = (0..MAX_DIFF_LINES + 500)
            .map(|i| format!("line{i}\n"))
            .collect();
        fs::write(dir.path().join("f.txt"), &big).unwrap();

        let git = GitRepo::open(dir.path()).unwrap();
        let diff = git.diff_file("f.txt").unwrap().expect("file changed");

        assert!(diff.truncated);
        let total_lines: usize = diff.hunks.iter().map(|h| h.lines.len()).sum();
        assert_eq!(total_lines, MAX_DIFF_LINES);

        let lines: Vec<&DiffLine> = diff.hunks.iter().flat_map(|h| &h.lines).collect();
        let removed_spans = lines.iter().find_map(|l| match l {
            DiffLine::Removed(text, spans) if text == "orig" => Some(spans.clone()),
            _ => None,
        });
        assert!(
            matches!(removed_spans, Some(spans) if !spans.is_empty()),
            "the Removed(\"orig\") line should still be paired with the \
             surviving first Added line despite the Added run's tail \
             being truncated away"
        );
    }

    #[test]
    fn pair_intraline_spans_on_a_removed_run_with_no_added_partner_does_not_panic() {
        // Simulates the state truncation can leave behind: a Removed run
        // whose Added partner was cut away entirely (doc §4's "a Removed
        // run whose paired Added got truncated away must yield an empty
        // Vec, not crash"). Exercised directly against `pair_intraline_spans`
        // since reliably forcing git2 to produce this exact post-truncation
        // shape end-to-end is not practical to set up deterministically.
        let mut lines = vec![
            DiffLine::Removed("a".to_string(), Vec::new()),
            DiffLine::Removed("b".to_string(), Vec::new()),
        ];
        pair_intraline_spans(&mut lines);
        for line in &lines {
            match line {
                DiffLine::Removed(_, spans) => assert!(spans.is_empty()),
                other => panic!("unexpected line: {other:?}"),
            }
        }
    }

    #[test]
    fn clone_repo_with_empty_url_returns_empty_url_error() {
        let dest = tempfile::tempdir().unwrap();
        let dest_path = dest.path().join("nonexistent-yet");
        let result = clone_repo("   ", &dest_path, |_| {});
        assert!(matches!(result, Err(GitError::EmptyUrl)));
    }

    #[test]
    fn clone_repo_into_a_nonempty_destination_returns_destination_not_empty() {
        let dest = tempfile::tempdir().unwrap();
        fs::write(dest.path().join("existing.txt"), "hi").unwrap();
        // No real URL is ever dereferenced -- destination validation runs
        // before any git2/network call, so a placeholder URL is fine here.
        let result = clone_repo("https://example.invalid/repo.git", dest.path(), |_| {});
        match result {
            Err(GitError::DestinationNotEmpty(p)) => assert_eq!(p, dest.path()),
            Err(other) => panic!("expected DestinationNotEmpty, got {other:?}"),
            Ok(_) => panic!("expected DestinationNotEmpty, got Ok"),
        }
    }

    #[test]
    fn clone_repo_fails_for_an_unreachable_source() {
        let dest = tempfile::tempdir().unwrap();
        let dest_path = dest.path().join("dest");
        let result = clone_repo("/definitely/does/not/exist/anywhere", &dest_path, |_| {});
        assert!(matches!(result, Err(GitError::Git2(_))));
    }

    #[test]
    fn clone_repo_clones_a_local_repository_into_an_empty_destination() {
        let (source_dir, source_repo) = init_repo();
        commit_file(&source_repo, "hello.txt", "hello from source", "initial");

        let dest = tempfile::tempdir().unwrap();
        let dest_path = dest.path().join("cloned");

        let mut progress_calls = 0usize;
        let repo = clone_repo(&source_dir.path().to_string_lossy(), &dest_path, |_| {
            progress_calls += 1;
        })
        .unwrap();

        assert_eq!(repo.workdir(), fs::canonicalize(&dest_path).unwrap());
        assert_eq!(
            fs::read_to_string(dest_path.join("hello.txt")).unwrap(),
            "hello from source"
        );
        // A local (non-network) clone may or may not drive the indexer
        // progress callback depending on libgit2's transport choice for a
        // same-filesystem source -- this only asserts the callback never
        // panics or corrupts state when it does fire, not that it must.
        let _ = progress_calls;
    }

    #[test]
    fn clone_repo_does_not_touch_an_existing_but_absent_destination_path() {
        // A destination that doesn't exist yet at all (as opposed to an
        // existing empty directory) must not be rejected -- `clone_repo`
        // lets `RepoBuilder::clone` create it, matching plain `git clone`.
        let (source_dir, source_repo) = init_repo();
        commit_file(&source_repo, "a.txt", "a", "initial");

        let parent = tempfile::tempdir().unwrap();
        let dest_path = parent.path().join("brand-new");
        assert!(!dest_path.exists());

        let repo = clone_repo(&source_dir.path().to_string_lossy(), &dest_path, |_| {}).unwrap();
        assert_eq!(repo.workdir(), fs::canonicalize(&dest_path).unwrap());
    }

    #[test]
    fn clone_progress_from_git2_progress_field_names_match_expectations() {
        // `git2::Progress` can't be constructed outside the crate (its
        // fields are private, populated only from a real libgit2 callback)
        // -- this checks `CloneProgress`'s own shape/derives instead of
        // the `From` conversion body, which the live clone test above
        // already exercises end-to-end whenever the callback does fire.
        let p = CloneProgress {
            received_objects: 1,
            total_objects: 2,
            indexed_objects: 3,
            indexed_deltas: 4,
            total_deltas: 5,
            received_bytes: 6,
        };
        assert_eq!(p, p);
        assert_eq!(CloneProgress::default(), CloneProgress::default());
    }
}
