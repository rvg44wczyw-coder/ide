# Git Log Viewer (E3)

## 1. Purpose

`E1`/`E2` already gave `ide-ui`'s Source Control panel a commit graph
(`GitPanel::graph`, backed by `GitRepo::commit_graph`), a commit-detail
popup (`GitRepo::commit_detail`), and a per-commit file diff
(`GitRepo::diff_commit`, shown via `GitPanel::diff`/`GitPanel::
select_commit`) — those three pieces of the JetBrains-family "Log" tab are
already done and this feature does not touch them again.

What's still missing is the part that makes a 500-commit graph actually
useful once a project has real history: narrowing it down. This feature
adds:

- **Filtering** the commit graph by branch, author, path, and date range,
  combinable (an AND of whichever fields are set).
- **Full-text search** over commit messages (summary + body), composable
  with the other filters as one more AND term.
- **"Show History of File"** — a `git log --follow` equivalent: the commits
  that touched a specific file, tracked across renames, as its own
  dedicated view rather than a filter combination (`--follow`'s semantics
  don't compose cleanly with the other filters — see §3.3).

Everything here reads history; nothing here writes to the repository.

## 2. Interface

### 2.1 `ide-core` additions (`crates/core/src/git/mod.rs`)

```rust
/// Combinable filter for `GitRepo::commit_graph`. Every `Some` field is
/// ANDed together; `CommitLogFilter::default()` matches every commit
/// (walking from `HEAD`, same as today's unfiltered graph).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommitLogFilter {
    /// Local branch name to walk from instead of `HEAD`. `None` means
    /// `HEAD` (today's behaviour, unchanged). Does not affect which
    /// commits an already-known `HEAD` sees relative to that branch —
    /// this changes the walk's *starting point*, not a post-hoc filter,
    /// the same way `git log <branch>` differs from `git log | grep`.
    pub branch: Option<String>,
    /// Case-insensitive substring match against the commit author's name
    /// *or* email (either matching is enough) — a user rarely knows
    /// whether they remember a colleague by name or by the email git logs
    /// them under, so requiring both would surprise them more often than
    /// help.
    pub author: Option<String>,
    /// Repository-relative path. A commit matches if its diff against its
    /// first parent (root commits: against an empty tree) touches this
    /// exact path — not a prefix/directory match, not rename-aware (see
    /// `file_history` in §2.1 below for the rename-following version).
    pub path: Option<PathBuf>,
    /// Inclusive lower bound, Unix seconds, compared against
    /// `CommitNode::timestamp` (commit time, not author time — matches
    /// what `commit_graph`/`commit_detail` already expose).
    pub since: Option<i64>,
    /// Inclusive upper bound, Unix seconds.
    pub until: Option<i64>,
    /// Case-insensitive substring match against the commit's full message
    /// (summary + body concatenated, i.e. the same text `git log --
    /// grep` searches) — not summary-only, so a search term that only
    /// appears in a commit's body still matches.
    pub query: Option<String>,
}

/// Safety bound on how many commits `commit_graph`'s revwalk will *visit*
/// while evaluating a filter, independent of `limit` (which bounds
/// *matches*). Without this, a narrow filter (e.g. a `query` that matches
/// nothing) against a large history would walk the entire commit graph
/// every call — cheap per-commit, but unbounded in aggregate the same way
/// `MAX_DIFF_FILES` bounds `diff_commit`'s per-call cost. Chosen well
/// above any repository this project's own dogfooding will produce, while
/// still bounding worst-case latency on a pathological repo (a shallow
/// clone of a huge upstream project, opened as a `ide` project root).
pub const MAX_COMMITS_SCANNED: usize = 200_000;

impl GitRepo {
    /// Commit graph reachable from `filter.branch` (or `HEAD` if `None`),
    /// newest first, matching every `Some` field of `filter`, capped at
    /// `limit` *matching* commits (visits at most `MAX_COMMITS_SCANNED`
    /// commits regardless of how many match — see that constant's doc).
    /// `commit_graph(limit, &CommitLogFilter::default())` is exactly
    /// today's unfiltered `commit_graph(limit)` — this method *replaces*
    /// the old single-argument one; every existing call site (including
    /// `GitPanel::refresh`'s and its reload path's own `commit_graph`
    /// calls in `crates/ui/src/git_panel.rs`) gains a
    /// `&CommitLogFilter::default()` argument, no behavioural change for
    /// those callers.
    ///
    /// Errors:
    /// - `GitError::Git2` if `filter.branch` is `Some` and doesn't resolve
    ///   to a local branch (propagated from `find_branch`, no fallback to
    ///   `HEAD` — a typo'd branch name should surface as an error, not
    ///   silently show the wrong history).
    ///
    /// Returns `Ok(vec![])`, not an error, for a repository with no
    /// commits yet (same "unborn HEAD is not an error" treatment the
    /// existing unfiltered walk already gives it).
    pub fn commit_graph(
        &self,
        limit: usize,
        filter: &CommitLogFilter,
    ) -> Result<Vec<CommitNode>, GitError>;

    /// History of `path` (repository-relative), newest first, following
    /// renames across the commit range the same way `git log --follow`
    /// does: each commit's diff against its parent is inspected with
    /// rename detection enabled (`git2::DiffFindOptions::new()` on the
    /// per-commit diff), and if `path` (as tracked through any detected
    /// renames so far) appears as either side of a changed file, that
    /// commit is included and the tracked path is updated to that
    /// commit's *old* name if this commit is where the rename happened.
    /// Capped at `limit` matches and `MAX_COMMITS_SCANNED` visits, same
    /// two-cap shape as `commit_graph`. Always walks from `HEAD` — no
    /// `branch`/other-filter composition (see §3.3 for why).
    ///
    /// Returns `Ok(vec![])` for a path never present in any commit
    /// reachable from `HEAD` (not an error — the same "not found is an
    /// empty result, not a failure" shape `conflicts()` and friends use
    /// elsewhere in this module).
    pub fn file_history(
        &self,
        path: impl AsRef<Path>,
        limit: usize,
    ) -> Result<Vec<CommitNode>, GitError>;
}
```

No new `GitError` variant — a filter that resolves to nothing is `Ok(vec![])`, not an error (§3.1).

### 2.2 `ide-ui` additions (`crates/ui/src/git_panel.rs` + `crates/ui/src/command.rs`)

```rust
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
    /// Set when applying produced a `GitError` (an unresolvable
    /// `branch`, or an unparsable `since`/`until`) — shown inline, same
    /// pattern `worktrees_popup.error`/`branches_popup`'s inline error
    /// already use. The graph is left as whatever it last successfully
    /// was, not cleared, on a failed apply.
    pub error: Option<String>,
    /// `true` while `GitPanel::graph` holds a `file_history` result
    /// instead of a `commit_graph` result — the filter bar is hidden
    /// while this is set (§3.3: file history doesn't compose with the
    /// other filters, so showing controls that don't apply would be
    /// misleading) and a "← Back to Log" affordance takes its place.
    pub viewing_file_history: Option<PathBuf>,
}

impl GitPanel {
    /// Rebuilds `filter` from `LogFilterState`'s text fields and reloads
    /// `graph` via the new two-argument `commit_graph`. On any parse/git
    /// error, sets `log_filter.error` and leaves `graph` untouched (§3.2).
    pub fn apply_log_filter(&mut self);

    /// Clears every `LogFilterState` field and reloads the unfiltered
    /// graph (`CommitLogFilter::default()`) — the toolbar's "Clear
    /// Filter" action.
    pub fn clear_log_filter(&mut self);

    /// Loads `path`'s rename-aware history into `graph` via
    /// `GitRepo::file_history` and sets `log_filter.viewing_file_history`.
    /// `path` is repository-relative (the caller — `CommandAction::
    /// ShowFileHistory`'s handler in `app.rs` — is responsible for
    /// stripping the project root off the active tab's absolute path
    /// before calling this, the same convention `diff_file`'s callers
    /// already follow).
    pub fn show_file_history(&mut self, path: &Path);

    /// Leaves file-history view and reloads the graph under whatever
    /// `LogFilterState` currently holds (not necessarily unfiltered — if
    /// the user had an active filter before switching to file history,
    /// returning restores it rather than discarding it).
    pub fn back_to_log(&mut self);
}
```

New `CommandAction::ShowFileHistory` (category `"Git"`, no default binding
— no JetBrains-family macOS binding exists for this action, confirmed by
the same `WebSearch`-verification precedent `E2`'s `GitWorktrees`/
`ToggleBlameAnnotations` entries already used, so per root `CLAUDE.md`'s
"never invent a binding" rule this is palette-only). Enabled exactly when
the active tab has a path inside an open git repository — mirrors
`is_command_enabled`'s existing `CommandAction::ToggleBlameAnnotations`
check (same "active tab, and it's a real git repo" precondition), not
`GitWorktrees`'s weaker "a project is open" one, since this command needs
a concrete file, not just a project.

## 3. Behaviour

### 3.1 Filter semantics

Every `Some` field on `CommitLogFilter` is ANDed. `branch` picks the walk's
starting point (not a post-filter); `author`/`query` are case-insensitive
substring matches; `path` is an exact repository-relative match against
either side of the commit's diff against its first parent (a commit that
only *renamed* the file without changing its content still counts — the
old and new paths both appear as diff sides); `since`/`until` are inclusive
bounds on commit time. `limit` bounds the number of *matches* returned;
`MAX_COMMITS_SCANNED` independently bounds how many commits the walk will
*visit* while looking for matches, so a filter matching nothing still
returns (empty, not an error) rather than walking forever on a huge
history — same shape `diff_commit`'s `MAX_DIFF_FILES` already established
for a different unbounded axis.

A `path` filter needs each visited commit's diff computed (parent tree vs.
this tree) purely to test membership — that diff is discarded afterward,
not attached to the returned `CommitNode` (unlike `file_history`, which
needs to track renames and so must inspect diffs with rename detection
turned on; plain `path` filtering does not enable rename detection, by
design — see §3.3 for why this is a deliberate, not missing, distinction).

### 3.2 Applying the filter bar

`LogFilterState`'s five text fields are free text until `apply_log_filter`
runs (bound to an explicit "Apply"/Enter action in the UI, not live-as-
you-type — a `path`/`query` filter needs a real diff/string-search pass
per candidate commit, so re-running it on every keystroke against a
500-commit-or-more graph is wasted work the user didn't ask for). `since`/
`until` are parsed as `YYYY-MM-DD` in the local timezone, converted to
Unix-seconds bounds; an unparsable date, or a `branch` that doesn't
resolve, sets `log_filter.error` and leaves `graph` as its last good state
— never a half-applied filter silently showing the wrong graph.

### 3.3 Why `file_history` doesn't compose with `CommitLogFilter`

`git log --follow` itself doesn't compose freely with most other `git log`
filters either (upstream git's own long-standing limitation) — `--follow`
needs to walk the *first-parent* history inspecting rename-vs-content
diffs at every step to decide which path to track next, which is a
fundamentally different traversal shape than "walk from a starting point,
test each visited commit against independent predicates." Modeling it as
one more `CommitLogFilter` field would either (a) silently ignore
`branch`/`author`/`date`/`query` whenever `path` was meant as "follow
this file" rather than "filter by this exact path," a foot-gun given the
plain `path` field already exists and means something different and
compatible with the other fields, or (b) require `CommitLogFilter` to grow
a `follow: bool` flag whose interaction with every other field would need
its own bespoke reasoning anyway. A separate method with its own simpler
contract (§2.1) is the smaller, more honest surface. `GitPanel` reflects
that split in its UI too (§2.2's `viewing_file_history`): the filter bar
and the file-history view are two distinct modes of the same graph list,
never shown active at once.

### 3.4 "Show History of File"

Invoked from the command palette (`Show History of File`) against the
active editor tab's path. Loads that file's rename-aware history into the
same graph list the filter bar drives, so commit selection, the existing
`commit_detail` popup, and `diff_commit`'s per-file diff (§1) all keep
working unmodified against a `file_history` result exactly as they do
against a `commit_graph` result — both are `Vec<CommitNode>`, and nothing
downstream of `GitPanel::graph` needs to know which produced it. A
"← Back to Log" control (visible only while `viewing_file_history` is
`Some`) calls `back_to_log`.

## 4. Constraints & invariants

- `commit_graph`'s new `filter` parameter is the same panic-free, error-
  as-`Result` contract every other `GitRepo` method already follows — no
  `unwrap`/`expect` on attacker-influenced data (a cloned repository's
  author name/commit message, in this case, since `author`/`query`
  matching runs against that content).
- `MAX_COMMITS_SCANNED` bounds walk cost independent of how selective
  `filter` is; `limit` bounds returned-vector size independent of how many
  commits exist. Both must be enforced together — capping only one still
  leaves the other axis unbounded.
- `file_history`'s rename detection reuses `git2::DiffFindOptions`, the
  same mechanism `diff_commit`/`diff_file`'s underlying diff-building
  already has available (not a new dependency, not a new capability this
  module didn't already need for something else).
- Thread-safety: unchanged from the module-level contract already stated
  at the top of `crates/core/src/git/mod.rs` — not safe to call
  concurrently against the same on-disk repository; the UI already
  serializes all `GitRepo` calls.

## 5. Examples

```rust
// Commits by "alice" touching src/lib.rs since 2026-01-01, newest 50.
// (`since`/`until` are plain Unix-seconds `i64`s -- however the caller
// computes one; `ide-ui`'s own conversion is `apply_log_filter`'s
// YYYY-MM-DD parse, §3.2. 1767225600 below is 2026-01-01T00:00:00Z.)
let filter = CommitLogFilter {
    author: Some("alice".to_string()),
    path: Some(PathBuf::from("src/lib.rs")),
    since: Some(1_767_225_600),
    ..Default::default()
};
let commits = repo.commit_graph(50, &filter)?;

// Full history of a file, following its renames.
let history = repo.file_history("src/moved_file.rs", 500)?;
```

```rust
// ide-ui: user types "fix panic" into the search box and hits Enter.
panel.log_filter.query = "fix panic".to_string();
panel.apply_log_filter(); // panel.graph now holds only matching commits
```

## 6. Dependencies & integration points

- Builds on `GitRepo::commit_graph`/`CommitNode` (E1) and `commit_detail`/
  `diff_commit` (E2) — no new type wraps `CommitNode`; filtered and
  file-history results are the exact same type the existing graph
  rendering, lane assignment (`assign_lanes`), and commit-detail/diff
  wiring already consume.
- `crates/ui/src/command.rs`: one new `Command`/`CommandAction` entry
  (`ShowFileHistory`), Git category (already exists since E2).
- No new Cargo dependency — `git2::DiffFindOptions` and `git2::Revwalk`
  are already in use elsewhere in `crates/core/src/git/mod.rs`.
- No new security-sensitive path: this feature only *reads* repository
  history (already-declared-sensitive per `CLAUDE.md`'s existing
  `crates/core/src/git/**` entry — untrusted repository content such as
  attacker-crafted author names/commit messages now also flows through
  `author`/`query` substring matching, covered by that entry's existing
  "treat everything a remote sends... as hostile input" language rather
  than needing a new bullet).

## Revision notes

- §2.1's `commit_graph` doc comment said it replaces "the old two-argument
  one" — the existing method is single-argument (`limit` only); fixed the
  wording and named the concrete `GitPanel` call sites (`refresh` and its
  reload path) that need updating for the signature change, so the `ui`
  role doesn't have to rediscover them by grep alone.
- §5's first example called an undeclared `unix_seconds(...)` helper that
  doesn't exist anywhere in the documented interface, so the example
  didn't actually compile against it. Replaced with a literal Unix-seconds
  `i64` (verified: `1_767_225_600` = 2026-01-01T00:00:00Z) plus a comment
  pointing at §3.2 for how `ide-ui` actually derives one from user input.
