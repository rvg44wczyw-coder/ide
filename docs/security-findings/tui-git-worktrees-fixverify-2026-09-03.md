# Hacker re-verification: T32 TUI Git Worktrees fix

## 1. Scope

Re-verification of `docs/security-findings/tui-git-worktrees-2026-09-03.md`
finding 1 (Medium: `WorktreeInfo::name`/`.branch`/`.path` render
unsanitized) against the fix on branch `rust-tui-dev/tui-git-worktrees`,
commit `3184998`.

`crates/tui/src/git_panel.rs`'s `git_panel` module is private (`mod
git_panel;` in `crates/tui/src/lib.rs`), unlike `ide-core`, so the
standalone external-crate harness technique used for the original pass
isn't available here. Verification instead combined: (1) reading the
actual diff (already confirmed by `rev`'s own grep that `refresh_worktrees`
is the only write path into `worktrees_popup.worktrees`, and that
`create_worktree`/`remove_worktree` both re-enter through it — no bypass);
(2) running the exact regression test live with `--nocapture` to confirm
it's a real reproduction, not a tautological check.

## 2. Live verification

```
cargo test -p ide-tui refresh_worktrees_strips_bidi_controls_from_name_branch_and_path -- --nocapture
```

Output confirms the raw `git` CLI itself accepted and visibly reordered a
bidi-override branch name during worktree creation (`Preparing worktree
(new branch 'feat‮ure')` — the reordering artifact is visible directly in
git's own terminal output, proof the codepoint is genuinely present and
genuinely affects rendering, not a theoretical claim), and the test's own
post-`refresh_worktrees` assertions (no `\u{202E}` in `name`/`path`/
`branch`) passed. The test's `evil_path`'s bidi codepoint sits in the
path's basename, which raw `git worktree add` uses as the registered
worktree name — so this one test exercises the codepoint reaching the
popup via all three fields (`name`, `path`, and the separately-crafted
`branch`) simultaneously, not just `name`.

Full suite re-run: `cargo test -p ide-tui` → 1070 passed, 0 failed — no
regressions from the fix.

## 3. Verdict

**Clean** — finding 1 is closed for this diff's own scope (the render-time
sanitization gap in `ide-tui`'s worktrees popup). The finding's
creation-time half (`add_worktree`'s `path` argument having no bidi check
in `crates/core/**`) remains open as a separate, cross-cutting `ide-core`/
`rust-core-dev` issue — correctly out of this role's scope, not silently
dropped; noted here so it isn't lost. It also still affects the
already-shipped `ide-ui` GUI popup (`crates/ui/src/app/render.rs`'s
`render_worktrees_popup`), unchanged by this run.
