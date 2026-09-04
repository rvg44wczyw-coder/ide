# Hacker findings: T37 Search and Replace in Path (ide-tui)

## Scope

Reviewed `rust-tui-dev`'s implementation of T37
(`docs/features/tui-search-and-replace-in-path.md`), worktree
`/Users/ivs/rust/ide-worktrees/rust-tui-dev-tui-search-and-replace-in-path`,
branch `rust-tui-dev/tui-search-and-replace-in-path`, commit `e9009b9`
(already `rev`-approved, two rounds). Root `CLAUDE.md`'s security-sensitive
list names this diff's own call site: `crates/tui/src/app.rs`'s
`confirm_replace_in_path_preview`/`apply_file_edits`, reaching
`ide_core::apply_workspace_edit_to_disk` with a regex/glob-driven,
user-typed-pattern-fed multi-file write.

**Attack-surface categories applied:**
- `InputValidation` / `PathTraversal` — yes, the core surface (glob/regex
  input driving a file-system walk and, ultimately, disk writes).
- `DoS` — yes (aggregate-size caps, regex engine choice).
- Race/TOCTOU (closest existing tag: `InputValidation`) — yes, explicitly
  called out by the doc itself at §3.3 as an accepted risk; verified
  independently rather than taken on faith.
- `MITM`/`Replay`/`Downgrade`/`KeyConfusion`/`Timing`/`WeakRandomness`/
  `SandboxEscape`/`MetadataLeak` — **ruled out**: this feature spawns no
  subprocess, opens no network/IPC connection, does no cryptography or key
  derivation, and stores no secrets. Confirmed by reading the full diff
  (`crates/tui/src/{search_panel,app,commands,ui}.rs`) — every new code
  path is pure in-process file I/O plus two background threads
  communicating over local `mpsc` channels.

**What's actually new in this diff vs. already-reviewed code:** the
underlying engine (`ide_core::search_tree_advanced`/`replace_in_path` in
`crates/core/src/search_in_path.rs`, and
`ide_core::apply_workspace_edit_to_disk` in
`crates/core/src/workspace_edit.rs`) is **unchanged** by this diff — zero
new `ide-core` API, confirmed during `rev`. This diff's own surface is
entirely `crates/tui/**`: a second background-thread state machine
(`SearchPanel::run_replace`/`poll_replace`, mirroring the existing
`run`/`poll` pair), a new `App::confirm_replace_in_path_preview`/
`apply_file_edits` call site into the already-hardened
`apply_workspace_edit_to_disk`, and new UI wiring
(`current_search_options`'s comma-split parsing of the `Include`/`Exclude`
text fields into `PathSearchOptions.include`/`exclude`).

**Live tests actually run** (not code-analysis-only): built a standalone
throwaway Rust binary in the local scratchpad (outside this repo, deleted
after use — nothing committed or left behind) with a path-dependency on
this worktree's `ide-core` crate, calling
`search_tree_advanced`/`replace_in_path`/`apply_workspace_edit_to_disk`/
`apply_transaction` directly with adversarial inputs mirroring exactly what
`current_search_options`/`confirm_replace_in_path_preview` construct and
pass to them. Five live probes run (details below); one code-analysis-only
confirmation (independent generation counters, point 5).

## Findings

1. **[InputValidation] Low** — `crates/core/src/search_in_path.rs`'s
   `build_matchers` unconditionally forces every exclude pattern through
   `format!("!{pattern}")` before handing it to `OverrideBuilder::add`. If
   the user's own exclude text already starts with `!` — a natural thing
   to type out of `.gitignore` muscle memory, meaning "un-exclude this" in
   gitignore syntax, but here just an ordinary character in a fresh
   pattern — the result is a doubled `!!pattern`, which `ignore`'s glob
   parser treats as cancelling the forced negation outright: **the pattern
   stops excluding anything at all**, silently, with no error.

   Verified live: a temp project with `keep.rs` and `drop.rs`, `search_
   tree_advanced` called with `exclude: vec!["!drop.rs".to_string()]`
   (exactly what `current_search_options` produces for an Exclude field
   containing the single token `!drop.rs`) returned **both** files as
   matches — `drop.rs` was not excluded, contrary to what a user typing
   `!drop.rs` into the Exclude field would reasonably expect.

   Concrete scenario: a user opens Replace in Path, types `node_modules`
   in Include and `!vendor` in Exclude believing (from gitignore habit)
   this protects `vendor/` from the bulk replace, confirms the preview
   without re-reading every listed file name closely, and `vendor/`'s
   files get rewritten anyway. Not an attacker-exploitable vulnerability
   (no escalation, no root escape, still bounded to the project tree,
   still gated behind an explicit preview-then-confirm step the user could
   in principle catch) — a silent-failure-of-user-intent bug on a bulk
   file-write path, which is why it's reported here rather than waved off
   as pure UX. Pre-existing in `ide_core::search_in_path` (unchanged by
   this diff, shared with `ide-ui`'s already-shipped C7 phase) — not a new
   regression introduced by `rust-tui-dev`'s port, but newly reachable
   from a second call site as of this diff.

   Suggested fix direction: in `build_matchers`, either reject an
   exclude pattern that already starts with `!` (surfacing a
   `PathSearchError::InvalidGlob`-shaped error the panel can show, per
   §3.5), or strip a single leading `!` from the user's own pattern before
   applying the forced negation — whichever direction, this belongs in
   `ide_core::search_in_path.rs`, not in either frontend's own text
   parsing.

## Verified clean (no finding)

- **PathTraversal / InputValidation (Include/Exclude → filesystem walk)**
  — live-tested `../../../etc/*`, `/etc/*`, comma-mixed traversal-looking
  patterns, and a brace-alternation glob shredded by the comma-split, all
  against a real temp project via `search_tree_advanced`. Every case
  either matched nothing or errored on the shredded-glob case (a real,
  already-documented usability gap — `docs/features/
  tui-search-and-replace-in-path.md` §3.6, not a security issue since it
  fails closed with `PathSearchError::InvalidGlob`, never open); no case
  ever named a path outside the temp root. This holds structurally, not
  just empirically: `walk_filtered` only ever recurses into `DirEntry::
  children` (confirmed by reading it), so a glob can narrow or exclude the
  already-root-confined candidate set but can never expand the walk beyond
  it — `ide_core`'s own test suite already has a test for exactly this
  (`traversal_looking_glob_pattern_only_narrows_never_escapes_root`) and
  my own independent run reproduces the same result outside that suite.
  `current_search_options`'s comma-split parsing (the new `ide-tui`-side
  code) doesn't change this: it only ever produces more glob strings fed
  one-by-one through the exact same `OverrideBuilder::add` validation loop
  every pattern already went through — no new path to smuggle anything
  past that validation.

- **DoS (catastrophic-backtracking regex)** — live-tested `(a+)+b` (a
  textbook exponential-blowup pattern against a backtracking engine)
  against 40 `a`s plus a non-matching tail: completed in 45.8µs. Rust's
  `regex` crate compiles to a linear-time finite automaton, not a
  backtracking matcher, so this class of DoS doesn't apply regardless of
  input — confirmed empirically rather than assumed from the crate's
  reputation. Replace's capture-group expansion (`buffer_search::
  replace_all`) reuses the same compiled query and the same linear-time
  guarantee; it introduces no additional regex-driven amplification beyond
  plain search.

- **DoS (unbounded preview size)** — `MAX_SEARCH_RESULTS` (1000, an
  aggregate cap shared with plain search) and `MAX_SEARCHABLE_FILE_BYTES`
  (5 MiB per file) are both pre-existing, unchanged caps. Live-tested: a
  1050-occurrence single file triggers `truncated: true` at exactly 1000
  recorded changes, and applying that truncated `ReplaceInPathResult`
  through `apply_workspace_edit_to_disk` produces file content that
  matches `apply_transaction`'s own output for that exact (truncated)
  `Transaction` — no silent over- or under-application relative to what
  the preview actually showed.

- **Race/TOCTOU (stale off-thread `Transaction` vs. a mutated file)** —
  the doc's §3.3 claim that the disk-write branch is "safe by
  construction" was independently verified live, not taken at face value:
  computed a `replace_in_path` preview against a file's original content,
  mutated the file on disk afterward (simulating a concurrent edit between
  preview and confirm), then applied the now-stale `Transaction` via
  `apply_workspace_edit_to_disk`. Result: rejected with
  `WorkspaceEditError::OffsetOutOfRange`, file left completely untouched.
  Also tested the multi-file rollback path specifically: a two-file edit
  where only the *second* file goes stale after the preview was computed
  still correctly restores the *first* file (already successfully
  written) back to its exact pre-call content — the existing
  read-write-same-handle fix from a prior hacker round
  (`docs/security-findings/rust-core-dev-workspace-edit-2026-08-20.md`,
  referenced directly in this function's own doc comment) is intact and
  unaffected by this diff, since `apply_workspace_edit_to_disk` itself is
  untouched — this diff only adds a second caller.

- **Concurrency (independent search/replace background state machines)**
  — code-analysis-only (no live stress test needed; see reasoning below).
  Read the entirety of `crates/tui/src/search_panel.rs`: `run`/`poll`
  (`generation: u64`, `rx: Option<Receiver<...>>`) and `run_replace`/
  `poll_replace` (`replace_generation: u64`, `replace_rx: Option<
  Receiver<...>>`) touch completely disjoint fields with zero shared
  mutable state — not even a shared lock. Separately, `App`'s own
  key-event dispatch (`handle_key`/`run_action`) is only ever invoked
  sequentially from the single-threaded main event loop (one `crossterm`
  event processed at a time); the only actual concurrency in this feature
  is between that main thread and up to two independent background worker
  threads (one per op), each isolated behind its own generation counter
  and channel. There is no code path — rapid key-mashing included — that
  could make the two state machines observe or corrupt each other's
  state, because nothing in either one's implementation ever reads or
  writes a field the other owns.

## Verdict

Findings (Low) — one Low-severity input-validation gap (exclude-pattern
double-negation footgun), pre-existing in `ide_core::search_in_path`
and newly reachable from this diff's call site; everything else tested
clean, including the disk-write TOCTOU path the doc itself flagged as a
risk worth independently verifying.
