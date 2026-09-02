# Security review: `ide-tui` git staging/branches/log-filters port (T28)

**Scope**: worktree `rust-tui-dev-tui-git-staging-branches-and-log-filters`, branch `rust-tui-dev/tui-git-staging-branches-and-log-filters`, commit `f1cc326` (rev-approved, both rounds). Reviewed against `CLAUDE.md`'s declared security-sensitive entry for `crates/tui/src/git_panel.rs`.

## Scope and rule-outs

This is a TUI port of already-shipped, already-hacker-reviewed `ide-ui` functionality: commit staging/amend (`git-commit-and-staging.md`, E1), the branch half of `git-branches-and-blame.md` (E2), and commit-log filtering plus file history (`git-log-viewer.md`, E3). `crates/tui/src/git_panel.rs`'s ported method bodies (`stage`/`unstage`/`request_discard`/`confirm_discard`/`commit`/`apply_log_filter`/`build_log_filter`/`clear_log_filter`/`show_file_history`/`back_to_log`/`open_branches_popup`/`close_branches_popup`/`checkout_branch`/`create_branch`/`request_delete_branch`/`confirm_delete_branch`/`merge_branch`, plus the `non_empty`/`parse_date_bound`/`days_in_month`/`days_from_civil` free functions) are copied near-verbatim from `crates/ui/src/git_panel.rs`; this pass does not re-derive findings for that already-audited logic, only re-verifies specific claims (see Findings/live tests below) rather than trusting the port blind.

The genuinely new surface this run added, all in `crates/tui/src/app.rs`:
- `handle_git_panel_key`'s precedence-chain dispatch and the new `handle_git_changes_key`/`handle_git_commit_message_key`/`handle_git_discard_confirm_key`/`handle_git_branches_key`/`handle_git_branches_filter_key`/`handle_git_branch_delete_confirm_key`/`handle_git_new_branch_key`/`handle_git_filter_key` sub-handlers — pure key dispatch, no I/O of their own.
- `App::filtered_branch_rows`/`clamp_branches_popup_selection` — the branch-list fuzzy filter added during implementation (not in the original doc, added this run per explicit user direction).
- `trigger_show_file_history`'s `Path::strip_prefix(&self.project_root)` to turn an already-canonicalized tab path into the repo-relative path `GitPanel::show_file_history` requires.

Attack-surface categories from the skill checklist, ruled in/out:
- **PathTraversal** — applies (stage/unstage/discard paths, `trigger_show_file_history`'s path stripping). Reviewed; see Findings.
- **InputValidation (adversarial)** — applies (branch names, log-filter date bounds, filter text). Live-tested (see below).
- **DoS** — applies (`parse_date_bound` against an adversarial digit run, `fuzzy_score` against a long typed filter). Live-tested / re-verified (see below).
- **SandboxEscape** — N/A. No subprocess is spawned anywhere in this diff; every write path goes straight through `ide_core::GitRepo`'s `git2`/libgit2 bindings (in-process), never a shelled-out `git` CLI. Confirmed via `grep -n "Command::new\|std::process" crates/tui/src/git_panel.rs crates/tui/src/app.rs` inside the diff's changed regions — no hits.
- **MetadataLeak** — N/A. No new subprocess argv, no new file written to disk by this diff (log-filter/branch-filter/commit-message text lives in in-memory `App`/`GitPanel` state only).
- Ruled out entirely, no applicable surface in this diff: MITM, Replay, Downgrade, KeyConfusion, Timing, WeakRandomness — no network handshake, no session/replay surface, no key derivation or crypto/random material anywhere in this feature.

Live tests actually run (not just reasoned about):
1. **DoS re-verification of `parse_date_bound`**: ran the existing regression test `parse_date_bound_rejects_an_unbounded_width_year_without_overflowing` (a 400-digit year string) via `cargo test -p ide-tui parse_date_bound_rejects_an_unbounded_width_year_without_overflowing` from the worktree — passes in under 1ms, confirming the `y.len() != 4 || m.len() != 2 || d.len() != 2` fixed-width check (which strictly precedes any `.parse::<i64>()` call) actually ported across from `docs/security-findings/git-log-viewer-ui-2026-09-02.md` finding 1, rather than assuming from a code comment.
2. **Branch-name adversarial input, against a real repository**: built a standalone scratch binary (`git2 = "0.19"`, matching this workspace's pinned major version) that inits a real repo, commits once, then calls `Repository::branch(name, &commit, false)` — the exact same libgit2 entry point `ide_core::GitRepo::create_branch` delegates to via `self.repo.branch(name, &target, false)?` — with 15 adversarial names: empty string, whitespace, `../../etc/passwd`, `a/../../b`, `-rf`, `--force`, a 100,000-character name, an embedded NUL byte, an embedded newline, `refs/heads/x`, `.lock`, `a..b`, `a.lock`, `~weird`, `a b c`. Result: `ok=1 err=14 total=15` — every case except a nested `refs/heads/x` name returned `Err` from libgit2's own ref-name normalization (invalid reference name / buffer-too-short / NUL-byte / not-a-valid-branch-name), with no panic, no hang, no out-of-bounds write. The one `Ok` case (`refs/heads/x` as the *branch* name) just creates a nested ref under `refs/heads/refs/heads/x` — still fully confined to the repository's own ref namespace, not a traversal outside it (this is standard git behavior — branch names may contain `/` to form hierarchical names like `feature/x`).
3. **`fuzzy_score` complexity**: read `crates/core/src/fuzzy.rs`'s doc comment and implementation directly — confirmed it's "a single left-to-right greedy pass... not a dynamic-programming search," O(pattern_len + candidate_len) per call. `App::filtered_branch_rows` calls it once per branch in `self.git.branches` per keystroke in the branch-filter text field; already the same usage shape as this crate's existing go-to-file/recent-files fuzzy filtering (larger candidate lists, already accepted). No new pathological-input class introduced.

Code-analysis-only (not independently live-tested, but traced end to end):
- Path provenance for `stage`/`unstage`/`request_discard`: all three take `&Path` values that, at every call site in `handle_git_changes_key`, come from `WorkingTreeStatus::staged`/`unstaged` `StatusEntry.path` fields — themselves populated by `git2::Status::path()` inside `ide_core::GitRepo::working_tree_status()`, never from raw string concatenation or user-typed text.
- `trigger_show_file_history`'s `path.strip_prefix(&self.project_root)`: both sides are already-canonicalized absolute paths (`project_root` fixed at project-open time, the tab path canonicalized when the tab was opened); `Path::strip_prefix` is component-aware, not a byte-prefix match, so a sibling directory like `/home/user/proj-evil` cannot spuriously match a `project_root` of `/home/user/proj`. No traversal-outside-root path reaches `GitPanel::show_file_history`.
- `branches_popup.filter`/`new_branch_name` typed text: `filter` is used *only* as the `pattern` argument to local, read-only `ide_core::fuzzy_score` calls against already-known branch names — never passed to any git operation. `new_branch_name` reaches exactly one call site, `GitRepo::create_branch(name, ...)`, which (per the live test above) safely rejects anything malformed via libgit2's own validation rather than this crate doing any ad hoc parsing/escaping itself.
- No call site in this diff builds a shell command string or passes user-typed text as an argument vector to a subprocess — grepped the diff's changed regions for `Command::new`/`std::process`/`sh -c` and found none; every write path is an in-process `git2` call.

## Findings

None. Every category investigated came back clean, both by direct code reading and by live testing where a live test was practical (date-bound DoS regression, branch-name adversarial-input fuzzing against a real libgit2-backed repository).

## Verdict

Clean — no findings.
