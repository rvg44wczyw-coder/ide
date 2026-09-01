# Language Auto-Detect v1

## 1. Purpose

`global-search-and-languages.md` added a user-editable list of
`(name, extension, command)` language configs (`custom_languages`), so a
non-Rust project can get LSP-backed features once the user opens
"Languages…" and types the config in by hand. Nothing today tells the
user that step exists, or that it would help — opening a Go project
today silently gets syntax highlighting (the regex tokenizer already
covers Go, `docs/features/syntax-highlighting.md`) and nothing else,
with no signal that a `gopls` config would unlock diagnostics/Find
Usages/hover/code actions/etc.

This feature closes that gap for the one case this app already knows how
to auto-detect *unambiguously*: a root marker file (`go.mod`, the same
shape Rust's own `Cargo.toml`-at-root check already uses) whose language
this app has a **known-good, zero-argument** default LSP command for.
When one is found and isn't already configured or previously dismissed, a
small popup offers to add it — "`go.mod` detected. Enable Go language
support (`gopls`)?" — Enable pushes the config into `custom_languages`
(exactly as if the user had typed it into "Languages…" themselves) and
re-runs detection immediately; Dismiss records the marker as declined for
this project so it doesn't ask again.

**Not in scope, and why:**

- **Syntax highlighting is unaffected.** It's already automatic and
  extension-driven for every built-in language (`ide_core::syntax`),
  including Go, with no config of any kind. This feature is entirely
  about the *optional* LSP-backed layer — the popup's copy says "language
  support", not "syntax highlighting", so it doesn't misrepresent what's
  actually gated on the user's answer.
- **Only markers with a verified zero-argument stdio command ship in
  v1.** `LanguageConfig::command` has always been a single program name,
  spawned via `Command::new(command)` with no argument vector
  (`global-search-and-languages.md` §1) — that's why the base feature's
  own doc uses `pyright-langserver --stdio` as the running example of
  what does *not* fit. The same constraint applies here: a marker only
  belongs in the built-in table if its language's real server is known to
  run correctly with zero arguments over stdio. `go.mod` → `gopls`
  qualifies (it's this codebase's own running example throughout prior
  docs). TypeScript (`typescript-language-server` requires `--stdio`),
  Java (`jdtls` requires workspace-data-dir arguments), and C/C++
  (`clangd` is zero-arg-friendly, but its natural marker `CMakeLists.txt`
  covers both `.c` and `.cpp`, and `LanguageConfig` supports exactly one
  extension per config — `global-search-and-languages.md` §1 already
  defers "more than one extension per language config") are all left out
  rather than shipping a suggestion likely to need editing (or picking an
  arbitrary single extension) right after the user clicks "Enable". The
  marker table (§2.1) is a flat list specifically so a future entry is a
  one-line addition once a given server's zero-arg behavior and
  single-extension fit are actually confirmed — not a redesign.
- **No "un-dismiss" UI.** Dismissing a marker in v1 is a one-way trip for
  that project; the user can still add the same config by hand via
  "Languages…" at any time (dismissal only suppresses the *automatic*
  popup, never the underlying feature). Same class of cut as
  `global-search-and-languages.md`'s "can't edit or remove the built-in
  Rust config" — an editable dismissal list is easy to add later and not
  worth the UI surface for a v1 whose only real leaf language is Go.
- **No re-scan on a filesystem-watcher tick.** Suggestions are
  recomputed on project load and on every tree refresh (§3.2) — both
  already-existing recompute points — not on some new independent timer.
  If a marker file appears between refreshes, the next one picks it up.

## 2. Interface / API

### 2.1 `ide-core` (`crates/core/src/language.rs`, additive)

```rust
/// One root marker file this build knows a default language-server
/// command for, distinct from `LanguageConfig::rust()`'s permanent,
/// non-optional special case above -- these are opt-in suggestions, not
/// another hardcoded detection path. `command` must be verified to run
/// correctly with zero arguments over stdio before an entry is added
/// here -- see this doc's §1 for why the list stays short.
struct LanguageMarker {
    /// No path separators -- checked as a direct child of the project
    /// root only, exactly like `detect_language`'s own `Cargo.toml`
    /// check, never a tree-wide scan.
    marker_file: &'static str,
    name: &'static str,
    extension: &'static str,
    command: &'static str,
}

const LANGUAGE_MARKERS: &[LanguageMarker] = &[
    LanguageMarker { marker_file: "go.mod", name: "Go", extension: "go", command: "gopls" },
];

/// One "`marker_file` exists at the project root -- want to add this
/// config?" suggestion, returned by `detect_language_suggestions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageSuggestion {
    pub marker_file: &'static str,
    pub config: LanguageConfig,
}

/// Every `LANGUAGE_MARKERS` entry whose `marker_file` exists directly
/// under `project_root` (`project_root.join(marker_file).exists()`,
/// mirroring `detect_language`'s `Cargo.toml` check bit-for-bit), in
/// `LANGUAGE_MARKERS` order. Pure detection -- does not consult
/// `custom_languages`, does not check whether Rust already claimed the
/// project, does not persist or dismiss anything. Typically returns zero
/// or one entry; a project with more than one recognized root marker
/// returns more than one, independently.
pub fn detect_language_suggestions(project_root: &Path) -> Vec<LanguageSuggestion> {
    LANGUAGE_MARKERS
        .iter()
        .filter(|m| project_root.join(m.marker_file).exists())
        .map(|m| LanguageSuggestion {
            marker_file: m.marker_file,
            config: LanguageConfig {
                name: m.name.to_string(),
                extension: m.extension.to_string(),
                command: m.command.to_string(),
            },
        })
        .collect()
}
```

`LanguageSuggestion`/`detect_language_suggestions` are re-exported from
`ide_core::lib.rs` alongside `LanguageConfig`/`detect_language`.

### 2.2 `ide-ui`

**`ProjectPreferences`** (`app.rs`) gains one field, persisted the same
way `custom_languages` already is:

```rust
struct ProjectPreferences {
    theme: Theme,
    custom_languages: Vec<LanguageConfig>,
    keymap: KeymapOverlay,
    format_on_save: bool,
    /// `LanguageSuggestion::marker_file` values the user has dismissed
    /// for this project (§3.3). `#[serde(default)]` at the container
    /// level (already present) means an older `preferences.json` missing
    /// this field just starts with an empty list -- no migration needed.
    dismissed_language_suggestions: Vec<String>,
}
```

`Default::default()` gets `dismissed_language_suggestions: Vec::new()`
alongside the other fields.

**`IdeApp`** gains:

```rust
/// Mirrors `ProjectPreferences::dismissed_language_suggestions`, synced
/// in `load_project_settings`/`flush_project_settings` exactly like
/// `custom_languages` already is.
dismissed_language_suggestions: Vec<String>,
/// Suggestions currently awaiting a user answer, filtered and ordered by
/// `refresh_language_suggestions` (§3.2). The popup (§3.4) always shows
/// `.first()`; resolving one (Enable or Dismiss) removes it from this
/// list, revealing the next if more than one marker matched.
pending_language_suggestions: Vec<ide_core::LanguageSuggestion>,
```

New methods:

```rust
/// Recomputes `pending_language_suggestions` from the current project
/// root, `custom_languages`, `dismissed_language_suggestions`, and
/// `active_language` (§3.2). No-op (clears the list) if no project is
/// open.
fn refresh_language_suggestions(&mut self);

/// "Enable" (§3.4): pushes `suggestion.config` into `custom_languages`
/// (identical effect to typing it into "Languages…" and clicking Add --
/// intentionally does not go through `add_custom_language`'s own
/// validation, since a marker-sourced config can never collide with an
/// existing extension or arrive with an empty field the way hand-typed
/// input can), removes `suggestion` from `pending_language_suggestions`,
/// and calls `redetect_language()` so the new config's LSP starts
/// immediately if it's now the detected language.
fn enable_language_suggestion(&mut self, suggestion: ide_core::LanguageSuggestion);

/// "Dismiss" (§3.4, and the popup's own close button/Escape -- §3.4):
/// appends `suggestion.marker_file` to `dismissed_language_suggestions`
/// (skipped if already present -- resolving the same marker twice in one
/// session, e.g. via a fast double click, must not duplicate the entry)
/// and removes `suggestion` from `pending_language_suggestions`. Does not
/// touch `custom_languages` or run `redetect_language`.
fn dismiss_language_suggestion(&mut self, suggestion: ide_core::LanguageSuggestion);
```

`render.rs` gains `render_language_suggestion_popup` (§3.4), called from
the main render loop immediately after `render_discard_confirm_popup` --
the closest structural match (a single-`Option`-shaped, two-button
confirm popup, not a list picker) among the existing popups.

## 3. Behavior

### 3.1 What ships in v1

One marker: `go.mod` → `Go` / `.go` / `gopls`. The mechanism (§2.1's
`LANGUAGE_MARKERS` table) is intentionally generic — a future language
is one more `LanguageMarker` entry plus one more `BUILTINS`-style test,
not a redesign — but this doc only vouches for `gopls`'s zero-argument
stdio behavior, which prior docs in this project already treat as
established (`global-search-and-languages.md`'s own running example).
Every other marker candidate considered is named and ruled out in §1
rather than silently omitted.

### 3.2 When suggestions are (re)computed

`refresh_language_suggestions` runs at both points `app.rs` already
recomputes the active language:

- `poll_tree_scan`'s `TreeScanKind::Load` arm, immediately after
  `self.redetect_language()` — a freshly opened project's first check.
- `poll_tree_scan`'s `TreeScanKind::Refresh` arm, after its existing
  inline detection logic — so a marker file that appears later (a `git
  pull`, a manual `Refresh` click) is caught on the next refresh, with no
  new timer or watcher hook.

Filtering, in order:

1. **No project open** → empty list, unconditionally.
2. **Rust already claimed the project** (`self.active_language`'s name
   is `"Rust"`, which `redetect_language`/the `Refresh` arm's own inline
   detection already computed by this point) → empty list. This app runs
   at most one LSP client per project (`global-search-and-languages.md`
   §1's "running more than one language server for the same project at
   once" is deferred), and Rust's `Cargo.toml` check always wins over
   every `custom_languages` entry (`detect_language`'s documented
   priority) — so a Go suggestion in a Rust-rooted repo could never
   actually activate. Suppressing it rather than offering a config that
   would sit inert in "Languages…" is the honest call, not a missing
   feature.
3. Otherwise, `ide_core::detect_language_suggestions(project.root())`,
   then drop any suggestion whose `marker_file` is already in
   `dismissed_language_suggestions`, and any whose `config.extension`
   case-insensitively matches an existing `custom_languages` entry
   (already configured — by this flow or by hand — so asking again would
   be pure noise).

### 3.3 Dismissal persistence

`dismissed_language_suggestions` lives in `ProjectPreferences`
(`.ide/preferences.json`), the same per-project file `custom_languages`
already persists to, flushed on the same cadence (`save()`, and before
switching away from the project in `load_project` — no new write path).
A dismissal is scoped to the project it was made in; opening a different
Go project asks again, matching the fact that `custom_languages` itself
is per-project too (a config added in one project was never global
either).

### 3.4 The popup

`render_language_suggestion_popup`: no-ops if
`pending_language_suggestions` is empty, else renders the first entry in
an `egui::Window::new("Language Detected")` (`.collapsible(false)`,
matching `render_discard_confirm_popup`'s shape):

- A label: `"{marker_file} detected. Enable {name} language support
  ({command})?"` — e.g. `"go.mod detected. Enable Go language support
  (gopls)?"`. Deliberately says "language support", not "syntax
  highlighting" (§1).
- Two buttons: **Enable** → `enable_language_suggestion`; **Dismiss** →
  `dismiss_language_suggestion`. The window's own close (`×`) button is
  treated the same as Dismiss (again matching
  `render_discard_confirm_popup`'s `cancel || !open` pattern) — closing
  without an explicit choice is a "not now", and in v1 "not now" and
  "don't ask again" are the same action (§1).

Not wired into the global Escape-priority chain (`handle_shortcuts`):
like `render_discard_confirm_popup`, this is a simple confirm dialog with
its own visible buttons and close control, not a list-navigation popup —
consistent with the precedent it's modeled on.

## 4. Invariants

- `detect_language_suggestions` never touches `custom_languages`,
  `dismissed_language_suggestions`, or the filesystem beyond one
  `Path::exists()` call per marker — every stateful decision (already
  configured, already dismissed, Rust active) is `ide-ui`'s filtering in
  `refresh_language_suggestions`, keeping the `ide-core` function a pure,
  trivially-testable predicate over a directory.
- `enable_language_suggestion`/`dismiss_language_suggestion` always
  remove the resolved suggestion from `pending_language_suggestions`
  before returning, so the popup can never get stuck showing an
  already-answered suggestion.
- A marker's `LanguageConfig` built by `detect_language_suggestions` can
  never collide with an existing `custom_languages` extension by the time
  `enable_language_suggestion` runs, because `refresh_language_suggestions`
  already filtered that case out in step 3 of §3.2 — `enable_language_
  suggestion` doesn't re-validate, unlike `add_custom_language`.
- **`detect_language_suggestions`'s marker check and `detect_language`'s
  ongoing custom-config matching use two different signals**, and this
  feature does not reconcile them: the marker check is root-file-existence
  (`go.mod` present), while `detect_language` (unchanged by this feature)
  matches a `custom_languages` entry by whether *any* file with its
  `extension` exists anywhere in the tree. Enabling a suggestion for a
  `go.mod`-only project with zero `.go` files pushes a config that won't
  actually activate until a `.go` file appears — not a bug introduced
  here (a hand-typed "Languages…" config for the same project would
  behave identically today), just a real, worth-naming consequence of
  building this feature on top of `detect_language`'s existing matching
  rather than changing it.

## 5. Worked example

Project root contains `go.mod`, a `.go` file, and no `Cargo.toml` (the
`.go` file matters: `detect_language`'s own ongoing re-detection for
`custom_languages` is tree-wide-**extension**-based, not marker-based —
see §4's note below — so a `go.mod` with no `.go` file anywhere gets
suggested and enabled but the LSP won't actually start until a `.go`
file exists; a realistic Go project has one from the start). First
load:
`redetect_language` finds no Rust marker and no matching
`custom_languages` entry yet, so `active_language` is `None` and the LSP
stays stopped. `refresh_language_suggestions` then finds `go.mod`, sees
`active_language` isn't Rust, sees no existing `.go` config and no prior
dismissal, and sets `pending_language_suggestions = [Go suggestion]`. The
popup shows "`go.mod` detected. Enable Go language support (`gopls`)?".

- Clicking **Enable**: `custom_languages` gains the `Go`/`go`/`gopls`
  config, the popup closes, `redetect_language` runs and starts `gopls`
  immediately (no reload needed).
- Clicking **Dismiss** (or closing the window): `dismissed_language_
  suggestions` gains `"go.mod"`, the popup closes, `custom_languages` is
  untouched, no LSP starts. Reopening the same project later:
  `refresh_language_suggestions` finds `go.mod` again but filters it out
  via the dismissal list — no popup. The user can still open
  "Languages…" and add the Go config by hand at any time.

## 6. Tests (required, per the implementing roles' own skills)

**`ide-core`** (`language.rs`): `detect_language_suggestions` — matches
`go.mod` at the root; ignores a nested `subdir/go.mod` (root-only, not a
tree-wide scan — this function takes a `Path`, not a scanned `DirEntry`,
so there's no tree to walk in the first place, but a test proving a
nested marker doesn't match is still worth having as a regression
guard); returns empty for a project with no recognized marker; returns
the exact expected `LanguageConfig` fields for the `go.mod` case.

**`ide-ui`** (`app.rs`): `refresh_language_suggestions` — no project open
→ empty; Rust-rooted project (`Cargo.toml` present) → empty even with a
`go.mod` also present; a marker already covered by an existing
`custom_languages` entry → filtered out; a previously-dismissed marker →
filtered out; the ordinary case → one suggestion. `enable_language_
suggestion` — pushes the config, clears it from the pending list, and
that `redetect_language` actually starts the LSP (reuse this crate's
existing `LspBridge` test-double pattern for asserting a start was
requested, the same one `add_custom_language`'s own tests already use).
`dismiss_language_suggestion` — appends to the dismissed list without
duplicating on a second call for the same marker, clears it from the
pending list, does not touch `custom_languages`. Round-trip: `Project
Preferences` serialize/deserialize includes `dismissed_language_
suggestions` (mirrors this file's existing `custom_languages` round-trip
test). Pure rendering (`render_language_suggestion_popup`'s own body) is
exempt from the coverage floor per this crate's established convention.

## 7. Diagrams

None — the flow is fully covered by §3's prose and §5's worked example;
this feature adds no new cross-crate protocol worth a sequence diagram
(everything happens inside `ide-ui`, on top of `ide-core` functions
`refactor-this.md`-style docs already show diagram-free when the surface
is this narrow).

## Revision notes

Self-review (doc + code, combined pass, performed inline rather than as a
separate role per this session's standing "no background agents"
instruction):

1. Confirmed `detect_language_suggestions` needed no filesystem access
   beyond `Path::exists()` and no dependency on `Project`/`DirEntry` —
   kept it a pure function over `&Path`, matching `detect_language`'s own
   `Cargo.toml` check exactly rather than inventing a second detection
   style.
2. While writing the `ide-ui` test for `enable_language_suggestion`,
   caught a real cross-mechanism gap before it could surprise a user: the
   marker check (root-file existence) and `detect_language`'s ongoing
   custom-config re-matching (tree-wide extension scan) are different
   signals, so enabling a suggestion for a `go.mod`-only project with no
   `.go` files anywhere doesn't actually start the LSP. Not a bug — a
   hand-typed "Languages…" config for the same project has identical
   behavior today — but real enough to call out explicitly; documented in
   §4 and §5 rather than silently left for someone to rediscover.
3. Deliberately suppressed suggestions in a Rust-rooted project (§3.2 step
   2) after tracing through what would happen otherwise: `detect_language`
   always prefers the `Cargo.toml` special case, so a `go.mod` suggestion
   in the same repo could never activate — offering it anyway would be a
   dead entry in "Languages…", not a helpful suggestion.
4. Verified against `CLAUDE.md`'s security-sensitive-paths list: this
   diff touches `crates/core/src/language.rs`, `crates/core/src/lib.rs`,
   `crates/ui/src/app.rs`, `crates/ui/src/app/render.rs` — none listed
   (notably not `crates/ui/src/lsp_bridge.rs`; the command that ends up
   spawned still flows through that already-audited path unchanged, and
   the value itself comes from this doc's own fixed, reviewed
   `LANGUAGE_MARKERS` table, never from arbitrary user input). No
   `hacker` pass required.
5. Coverage: `crates/core/src/language.rs` 100%, `crates/ui/src/app.rs`
   96.53% (workspace-wide `ide-ui` 81.49%, `ide-core` 97.40%).
   `crates/ui/src/app/render.rs`'s one new function
   (`render_language_suggestion_popup`) is pure rendering, exempt from
   the floor per this crate's established convention — the file's
   overall 8.43% is pre-existing and unrelated to this diff.
6. Full workspace `cargo fmt --all -- --check` /
   `cargo clippy --workspace --all-targets -- -D warnings` /
   `cargo build --workspace --all-targets` / `cargo test --workspace` all
   green.
