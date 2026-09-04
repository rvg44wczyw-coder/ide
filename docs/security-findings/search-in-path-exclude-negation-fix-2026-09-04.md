# Hacker findings: rust-core-dev exclude-negation fix (targeted pass)

## Scope

Reviewed `rust-core-dev`'s standalone security-fix branch, worktree
`/Users/ivs/rust/ide-worktrees/rust-core-dev-search-in-path-exclude-negation-fix`,
branch `rust-core-dev/search-in-path-exclude-negation-fix`, final commit
`767f3f3` (built on `884c44c` and `c6ef60f`, both already `rev`-approved
in earlier rounds; `767f3f3` was produced *during* this hacker pass itself
and is also now `rev`-approved). Root `CLAUDE.md` names
`crates/core/src/search_in_path.rs` explicitly as security-sensitive
(parses user-typed include/exclude glob patterns feeding a
`WorkspaceEdit`), so this diff needs its own hacker pass before merge,
independent of the fact that it's itself fixing an earlier hacker
finding (`docs/security-findings/tui-search-and-replace-in-path-2026-09-04.md`,
finding 1, from a sibling worktree).

This is a **targeted** pass, not a full re-run of the broader
path-traversal/DoS/TOCTOU/concurrency review already done for this whole
feature area in `tui-search-and-replace-in-path-2026-09-04.md` — that
review's conclusions on those categories are unaffected by this diff's
narrow scope (only `build_matchers`'s include/exclude loops changed).

**Attack-surface categories applied:** `InputValidation` (the only
relevant category for this diff's scope — a targeted adversarial probe of
one function's glob-handling edge cases). `PathTraversal`/`DoS`/`MITM`/
`Replay`/`Downgrade`/`KeyConfusion`/`Timing`/`WeakRandomness`/
`SandboxEscape`/`MetadataLeak` are out of scope for this pass (already
covered for this whole module in the prior findings doc; this diff
doesn't touch the walk/traversal logic, only the glob-string
preprocessing at the top of `build_matchers`).

**Live tests actually run:** two throwaway probe binaries in the local
scratchpad (outside the repo, path-depending on this worktree's
`ide-core`, deleted after use — nothing committed or left in the
worktree): one testing the leading-`!` fix and bypass attempts, one
isolating and confirming a second bug this pass discovered (empty-string
exclude), both run against real temp-directory projects via
`search_tree_advanced`.

## Findings

None outstanding. One finding was discovered mid-pass and is already
fixed on this same branch (commit `767f3f3`, `rev`-approved) — recorded
below for the record, not as an open item.

**Resolved during this pass — [InputValidation] Low (fixed in `767f3f3`):**
`build_matchers`'s exclude loop's `starts_with('!')` guard (added in
`884c44c` to fix the original bang-cancellation bug) doesn't catch an
*empty-string* pattern. `format!("!{}", "")` produces the bare string
`"!"`, which `ignore::gitignore::GitignoreBuilder::add_line` treats
exactly like any other bare `!`: consumes it as the forced negation
marker, leaves an empty remainder, which compiles (after the `**/`
auto-prefix for a pattern with no literal `/`) to a glob matching every
path. Net effect: `exclude: vec!["".to_string()]` silently excludes
*every* file, the same class of danger as the original bug (a forced `!`
interacting with `ignore`'s line-prefix parsing to produce a
much-broader-than-intended effect), just reached through a different
input shape the first fix didn't cover.

Verified live: a throwaway probe against a real 2-file temp project
returned 0 matches with `exclude: vec!["".to_string()]` (both files
excluded), versus 2 matches with an equivalent empty *include* pattern
(accidentally benign — confirmed separately). Not reachable through
either shipped frontend today (`current_search_options`/`split_glob_list`
already filter blank glob-list segments out of the `Vec` before it
reaches `PathSearchOptions`), so this is `ide_core`'s own public-API
robustness gap, not a live user-facing regression — same reasoning
already applied to the original leading-`!` finding.

Fixed (same commit, already `rev`-approved): both the include and exclude
loops in `build_matchers` now skip an empty-string pattern via
`if pattern.is_empty() { continue; }`, treating it as a no-op rather than
either erroring or silently misbehaving. New tests
`empty_exclude_pattern_is_ignored_not_treated_as_exclude_everything` and
`empty_include_pattern_is_ignored` cover both directions.

**Bypass attempts against the leading-`!` check — all ruled out, no
finding:** live-tested a battery of adversarial exclude strings intended
to reproduce the original "forced `!` gets silently cancelled" class of
bug through some avenue *other* than a literal leading `!`:
- Leading whitespace (`" !drop.rs"`, `"\t!drop.rs"`) — not rejected by
  `starts_with('!')` (correct — Rust's `starts_with` is byte-exact), but
  does **not** reproduce the danger: the resulting glob looks for a
  filename literally starting with a space/tab, which doesn't match
  `drop.rs`, so the pattern simply fails to exclude anything — the same
  (unexciting, expected) failure mode as any mistyped glob, not a silent
  cancellation of intent. Both shipped frontends already `.trim()` this
  field before building the `Vec`, so it's not reachable as typed anyway.
- Fullwidth Unicode bang (`'！'`, U+FF01) — treated as an ordinary literal
  character, not recognized as `!` by either `starts_with('!')` or
  `ignore`'s ASCII-specific line-prefix parser. No special meaning either
  way; same "doesn't match, doesn't exclude" non-issue as above.
- Bang mid-pattern (`"a!drop.rs"`) — never had any special meaning (only a
  *leading* `!` is negation-relevant in gitignore syntax); confirmed inert.
- Double bang (`"!!drop.rs"`) — correctly rejected (`starts_with('!')` is
  true), no bypass.
- Backslash-then-double-bang (`"\\!!drop.rs"`) — passes the check (starts
  with `\`, not `!`), forced to `"!\!!drop.rs"`; the outer `!` is consumed
  as the (correct, intended) forced negation, the user's own `\!!` becomes
  two literal `!` characters after glob-level backslash-escaping — pattern
  compiles to matching a filename literally starting with `!!`, doesn't
  match `drop.rs`, no exclusion happens. Not a reproduction of the
  cancellation bug (only one `!` was ever consumed as negation).

None of these reproduce the original silent-cancel-everything mechanism;
the one genuine gap found (empty string) is unrelated to any of these
variants and is already fixed as described above.

**Panic/hang surface — clean, no finding:** live-tested (via
`std::panic::catch_unwind` around every case) an empty string, a bare
`"!"`, a bare `"\\"` (dangling escape — correctly rejected by `ignore`'s
own parser with `Error::Glob`, pre-existing behavior, not from this fix),
a 100,001-byte pattern (`"!"` + 100,000 `'a'`s), a pattern containing a
lone Unicode combining mark, and a pattern containing an embedded NUL
byte. No panics, no hangs; every case returned a normal `Ok`/`Err` value
promptly. The long-string case's `Display` output for the resulting
`PathSearchError` does echo the full malformed pattern back verbatim
(~200KB in this synthetic case) — this is pre-existing behavior shared by
every other `PathSearchError::InvalidGlob` in this module (the `glob`
field is always the user's own input, echoed back for debuggability), not
something this diff introduces or changes, and is bounded by whatever
practically limits a UI text field's length rather than by this error
path itself; not filing as a new finding since it doesn't change behavior
this diff owns.

**Escape-hatch design question (item 4 of the original ask) — not a
security finding, folded into `rev`'s own review:** the
`\!important.txt` backslash-escape mechanism `c6ef60f` documents and the
`[!]important.txt` bracket-class alternative it rules out were both
independently re-verified live during this pass (same results as
`c6ef60f`'s own verification): backslash-escape works and excludes
exactly the literal filename; bracket-class errors as an unclosed
character class. No better/cheaper alternative was found. This was
already the subject of `rev`'s approval on `c6ef60f`, not a hacker-only
concern — recorded here only for completeness since the original task
prompt asked this pass to sanity-check it too.

## Verdict

Clean — one Low-severity finding (empty-string exclude silently matching
everything) was discovered mid-pass and is already fixed and
`rev`-approved on this same branch (`767f3f3`) before this findings doc
was finalized; every other adversarial variant tried against the same
code path came back clean, and the panic/hang and escape-hatch checks
found nothing new.

CHAIN_STEP step=hacker result=clean
