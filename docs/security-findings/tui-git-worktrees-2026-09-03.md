# Hacker findings: T32 TUI Git Worktrees

## 1. Scope

- Reviewed: `crates/tui/src/git_panel.rs` (`WorktreesPopupState`,
  `WorktreeAddField`, `open_worktrees_popup`/`close_worktrees_popup`/
  `refresh_worktrees`/`create_worktree`/`remove_worktree`), `crates/tui/
  src/app.rs` (`trigger_git_worktrees`, `handle_git_worktrees_key`,
  `handle_git_worktree_add_key`, `handle_git_worktree_remove_confirm_key`),
  and `crates/tui/src/ui.rs` (`render_git_worktrees_popup`), branch
  `rust-tui-dev/tui-git-worktrees`, commit `0d1ad05`. `rev` already
  approved this diff (quality + a first security-checklist pass).
- Attack-surface categories, ruled in/out:
  - **Input validation / metadata leakage (UI spoofing)** — applies
    directly: this diff is a near-verbatim port of `ide-ui`'s worktrees
    popup, calling the same `ide_core::GitRepo::add_worktree`/
    `remove_worktree`/`worktrees()` a prior core-level hacker pass
    (`docs/security-findings/git-worktrees-core-2026-09-01.md`) already
    flagged a Medium bidi-override finding against. In scope, live-tested.
  - **Sandbox/subprocess escape** — ruled out: no subprocess spawned by
    this diff; everything goes through `git2`/libgit2 in-process (test
    code shells out to the real `git` CLI only for fixture setup, not a
    production code path).
  - **DoS/resource exhaustion** — applies narrowly (rendering an
    attacker-influenced-length worktree list/path); live-tested.
  - **MITM/Replay/Downgrade/KeyConfusion/Timing/WeakRandomness/
    PathTraversal** — ruled out: no network protocol, no session/replay
    surface, no crypto, no timing-sensitive comparison, no randomness; the
    path-traversal-shaped checks (`WorktreeInsideRepo`, destination-empty)
    live entirely in `crates/core/src/git/mod.rs`, unchanged by this run
    and already covered by the prior core-level pass.
- Live tests actually run (not code-analysis-only): a standalone scratch
  Rust binary (`wt_hacker`, built in `/private/tmp/.../scratchpad/
  wt_hacker`, **outside** this worktree/repo, depending on this worktree's
  `ide-core` via a path dependency — never written into the reviewed
  worktree itself, per this skill's "never write any file except your own
  findings doc" rule) driving `GitRepo::add_worktree`/`remove_worktree`/
  `worktrees()` against real temp repos created via the real `git` CLI:
  (1) a bidi-override codepoint in `add_worktree`'s `path` argument
  (distinct from `name`, which the prior core pass already covers); (2) 50
  real worktree add/list cycles, timed; (3) a 100,000-character single
  path component passed to `add_worktree`. Also read `crates/ui/src/app/
  render.rs`'s `render_worktrees_popup` (the already-shipped GUI popup
  this TUI code is a "near-verbatim port" of) to check whether the prior
  Medium finding's suggested fix was ever actually applied downstream.

## 2. Findings

### 1. [InputValidation / MetadataLeak, Medium] `WorktreeInfo::name`/`.branch`/`.path` all render unsanitized in the new TUI popup — the prior core-level finding's suggested fix was never applied anywhere downstream, and `.path` has a wider, more directly reachable gap than `.name`/`.branch` ever did

**Location**: `crates/tui/src/ui.rs`'s `render_git_worktrees_popup` (this
diff) — `format!("{}  {}  {}", wt.name, branch, wt.path.display())` — plus,
for context, `crates/ui/src/app/render.rs:2080-2086`'s already-shipped GUI
`render_worktrees_popup`, which has the identical gap.

**Background**: `docs/security-findings/git-worktrees-core-2026-09-01.md`
finding 1 already established that `WorktreeInfo::name`/`.branch` can
carry an unterminated Unicode bidi-override codepoint (the "Trojan
Source"/CVE-2021-42574 class) into a rendered popup, and explicitly
recommended "whoever implements the UI role's rendering of
`WorktreeInfo::name`/`.branch` should reuse the existing
`strip_bidi_controls` helper... rather than assuming `add_worktree`'s own
input validation is the only path data can arrive through." I checked
whether that recommendation was ever acted on: **it was not** — the
already-shipped GUI popup (`crates/ui/src/app/render.rs:2080-2086`)
renders `worktree.name`/`branch`/`worktree.path.display()` with no
sanitization step at all, and this new TUI port faithfully copies that
same gap rather than closing it, since T32's own doc (`docs/features/
tui-git-worktrees.md`) never mentions sanitization either.

**New finding beyond the prior one**: `.path` is not just "also
unsanitized at render," it has a *wider creation-time gap* than `.name`
ever did. `add_worktree`'s current validation
(`crates/core/src/git/mod.rs`, the `has_bidi_control` check) inspects only
`name` — `path` receives **no bidi/control-character check at all** — and
unlike a `name`-only attack (which requires either an external tool or a
user directly typing the invisible codepoint into the "Name" field),
`path` is reachable through this app's own "Add Worktree" form's `Path`
field / the GUI's own "Browse…"-or-typed path field just as easily, since
nothing about typing or pasting a path rejects invisible bidi characters.

**Attack scenario**: A user (attacker or a socially-engineered victim
copy-pasting a path from somewhere untrusted, e.g. a README/issue-tracker
comment with a crafted "helpful path" containing an invisible RTL
override) types or pastes a path like `/Users/victim/projects/good\u{202E}
evil` into the Add Worktree form's Path field, leaving Name/Branch
ordinary. `create_worktree` → `add_worktree("goodname", path, None)`
succeeds outright (verified live, see below). The next time the popup
lists worktrees, `wt.path.display()` renders the raw override codepoint,
which can visually reorder everything rendered after it in that row (and,
depending on terminal/font bidi handling, potentially make a
`[locked]`/"press r again to force remove" suffix or an adjacent row's
content read as something else) — the same UI-spoofing class already
fixed once for `blame_gutter.rs`/`git_panel.rs::commit_detail` during E2's
hacker pass, and explicitly flagged (for `.name`/`.branch`) but never
actually closed for this feature.

**Verified live**: `repo.add_worktree("goodname", "<tmp>/good\u{202E}
evil", None)` returned `Ok(())`; the resulting `WorktreeInfo.path` came
back from `repo.worktrees()` containing the literal `\u{202E}` codepoint,
confirmed via a live check (`path.to_string_lossy().chars().any(...)` over
the same bidi-control character class `add_worktree`'s own `name` check
already uses) — see this run's scratch harness output:
```
Calling add_worktree with a bidi-override codepoint in PATH (not name)...
add_worktree result: Ok(())
worktree name="goodname" path="...good\u{202e}evil" contains_bidi_override=true
```

**Realistic capability model**: same as the prior finding's — local-only,
not remote/clone-triggerable (worktree registrations never travel over
`clone`/`fetch`/`push`), but reachable through this app's own UI in one
step (paste a path), not just via an external tool or a manually-run `git`
CLI.

**Suggested fix direction**: two independent layers, matching the prior
finding's own two-layer recommendation:
1. Creation-time: extend `add_worktree`'s existing `has_bidi_control`
   check (currently `name`-only) to also reject the same character class
   in `path`'s string representation, closing the self-inflicted case at
   the source this app controls.
2. Render-time (belt-and-suspenders, since `worktrees()` can still surface
   a pre-existing bad entry created by some other local tool or an older
   binary): both `render_git_worktrees_popup` (this diff) and the
   already-shipped `render_worktrees_popup` (GUI) should run `wt.name`/
   `wt.branch`/`wt.path.display()`'s string form through the existing
   `strip_bidi_controls` helper before formatting, the same way
   `GitPanel::commit_detail` already does for commit summary/author text.
   Since the GUI's own instance of this gap predates this run and isn't
   this diff's own regression, it's noted here for completeness but the
   actionable fix for *this* chain run is `render_git_worktrees_popup`
   specifically.

### 2-4. Other categories checked, no findings

- **DoS — many worktrees**: added 50 real worktrees and timed
  `GitRepo::worktrees()`: 315ms to add 50 (real `git2` worktree-creation
  cost, not this diff's concern), 8.6ms to list all 50 — linear, no
  quadratic blowup, no hang. `render_git_worktrees_popup`'s own list
  construction is a single `O(n)` `.map()`/`.collect()` with no recursion;
  bounded in practice by how many real worktree registrations an attacker
  could get created, which requires actual disk I/O per entry, not a cheap
  remote-triggerable amplification. Clean.
- **DoS — extremely long path**: a 100,000-character single path
  component passed to `add_worktree` failed cleanly with an OS-level
  "failed to stat" error (`ENAMETOOLONG`-shaped) in under 10ms — no panic,
  no hang, no resource exhaustion. (The analogous long-`name` case was
  already covered by the prior core-level pass — clean there too, via
  `git2`'s own buffer-size guard.) Clean.
- **`open_worktrees_popup`'s defensive re-open-if-no-repo path**: this is
  single-threaded, synchronous UI-driven state (no background thread, no
  concurrent actor that could race the check-then-use), so classic TOCTOU
  doesn't apply; the "re-open if `self.repo.is_none()`" branch is the same
  already-accepted pattern `open_branches_popup` already established
  (unchanged by this diff). No new issue.

## 3. Verdict

**Findings (Medium)** — one Medium: `WorktreeInfo::name`/`.branch`/`.path`
render fully unsanitized in the new TUI worktrees popup, a live-confirmed
extension of a previously-known-but-never-actually-fixed Medium finding,
with `.path` newly shown to have no creation-time validation at all
(wider gap than `.name`/`.branch`).
