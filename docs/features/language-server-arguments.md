# Language Server Arguments v1

## 1. Purpose

`global-search-and-languages.md` and `language-auto-detect.md` both
documented the same v1 gap: `LanguageConfig::command` is a single program
name, spawned via `Command::new(command)` with **no argument vector** —
fine for a server that runs correctly with zero flags over stdio
(`rust-analyzer`, `gopls`, `pylsp`, `clangd`), but unusable for one that
needs an explicit flag (`typescript-language-server --stdio`) or a
required argument (`jdtls -data <path>`). Both docs named this as a
deliberate, accepted cut rather than a bug.

This feature lifts it: `LanguageConfig` gains an `args: Vec<String>`
field, threaded through `ide_lsp::LspClient::start_with_command` and
`ide-ui`'s "Languages…" settings window, so a user (or the auto-detect
popup) can configure a server that needs flags. It is still an explicit
argv — `Command::new(command).args(args)` — never a shell string; §4
covers exactly what does and doesn't change about the trust model this
opens.

**Also in this pass** (small, uses the new field directly): two more
entries in `language-auto-detect.md`'s marker table —

- **Python**: `pyproject.toml` **or** `setup.py` **or** `requirements.txt`
  (first one found wins — a project only needs one, and asking three
  times for the same "Python" answer would be noise) → `pylsp`, no args.
- **TypeScript**: `tsconfig.json` → `typescript-language-server --stdio`.
  Deliberately *not* `package.json` — that file exists for any Node
  project, including plain JavaScript ones with no TypeScript at all;
  `tsconfig.json` is the marker that actually implies TypeScript.

**2026-08-28 addendum superseded the two bullets that used to be here**
(C/C++ and Java were originally left out of auto-detect for the reasons
below) — see the Revision notes' 2026-08-28 entry for the resolution. Kept
here as the original reasoning, since the underlying tension is still real
even though the decision changed:

- **C/C++**: `clangd` needs no args and would be a clean addition
  argv-wise, but `LanguageConfig` still supports exactly one `extension`
  per config (`global-search-and-languages.md` §1's other, still-open v1
  cut), and `CMakeLists.txt` doesn't reliably imply "this project has a
  `.cpp` file" any more than it implies "`.c`" — a C-only or C++-only
  project would suggest the wrong extension roughly half the time, hitting
  exactly the "config added but its language never re-activates" gap
  `language-auto-detect.md` §4 already documents for the `go.mod`-with-no-
  `.go`-files case, at much higher odds of actually happening.
- **Java**: `jdtls` needs `-data <path>`, and that path is a real,
  per-installation/per-workspace directory with no universal default this
  app could fabricate (unlike `--stdio`, which is a fixed literal for
  every TypeScript project everywhere). Guessing one would be worse than
  not suggesting at all.

## 2. Interface / API

### 2.1 `ide-core` (`crates/core/src/language.rs`)

```rust
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LanguageConfig {
    pub name: String,
    pub extension: String,
    pub command: String,
    /// Argv entries passed to `command`, in order, after the program name
    /// itself -- e.g. `["--stdio"]`. Empty for a server that runs
    /// correctly with zero arguments (most of them). `#[serde(default)]`
    /// so a `custom_languages` entry persisted by a build before this
    /// feature (no `"args"` key in its JSON) still deserializes as
    /// `args: vec![]` instead of failing to load.
    #[serde(default)]
    pub args: Vec<String>,
}
```

`LanguageConfig::rust()` sets `args: Vec::new()`.

```rust
struct LanguageMarker {
    marker_files: &'static [&'static str], // first one present wins
    name: &'static str,
    extension: &'static str,
    command: &'static str,
    args: &'static [&'static str],
}

const LANGUAGE_MARKERS: &[LanguageMarker] = &[
    LanguageMarker { marker_files: &["go.mod"], name: "Go", extension: "go", command: "gopls", args: &[] },
    LanguageMarker {
        marker_files: &["pyproject.toml", "setup.py", "requirements.txt"],
        name: "Python", extension: "py", command: "pylsp", args: &[],
    },
    LanguageMarker { marker_files: &["tsconfig.json"], name: "TypeScript", extension: "ts", command: "typescript-language-server", args: &["--stdio"] },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageSuggestion {
    /// The specific marker file that actually matched (e.g. `"setup.py"`
    /// when a project has that but not `pyproject.toml`) -- an owned
    /// `String` now, not `&'static str`, since which one matched is a
    /// runtime fact, not a fixed table value.
    pub marker_file: String,
    pub config: LanguageConfig,
}

pub fn detect_language_suggestions(project_root: &Path) -> Vec<LanguageSuggestion> {
    LANGUAGE_MARKERS
        .iter()
        .filter_map(|m| {
            let matched = m
                .marker_files
                .iter()
                .find(|f| project_root.join(f).exists())?;
            Some(LanguageSuggestion {
                marker_file: matched.to_string(),
                config: LanguageConfig {
                    name: m.name.to_string(),
                    extension: m.extension.to_string(),
                    command: m.command.to_string(),
                    args: m.args.iter().map(|s| s.to_string()).collect(),
                },
            })
        })
        .collect()
}
```

### 2.2 `ide-lsp` (`crates/lsp/src/client.rs`)

```rust
async fn spawn_child(command: &str, args: &[String], project_root: &Path) -> Result<Child, LspError> {
    Command::new(command)
        .args(args)
        .current_dir(project_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| { /* unchanged */ })
}

impl LspClient {
    pub fn start(project_root: impl AsRef<Path>) -> Result<Self, LspError> {
        Self::start_with_command(project_root, "rust-analyzer", &[])
    }

    pub fn start_with_command(
        project_root: impl AsRef<Path>,
        command: &str,
        args: &[String],
    ) -> Result<Self, LspError> {
        // unchanged body, `spawn_child(&command_owned, args, &project_root)`
    }
}
```

Every existing call site (production and test) passes `&[]` for the
previously-implicit empty argv.

### 2.3 `ide-ui`

- `LspBridge::start_with_command(&mut self, project_root: &Path, command: &str, args: &[String])`
  — forwards to `LspClient::start_with_command` unchanged otherwise. Its
  doc comment's "no argument vector" line is corrected to describe the
  new, still-shell-free trust boundary (§4).
- Every call site in `app.rs` (`redetect_language`, `poll_tree_scan`'s
  `Refresh` arm, `restart_lsp`) passes `&lang.args` instead of implicitly
  assuming none.
- `IdeApp` gains `new_language_args: String` (a fourth draft field,
  space-separated raw text, alongside `new_language_name`/`_extension`/
  `_command`). `add_custom_language` parses it via
  `.split_whitespace().map(str::to_string).collect()` — naive
  whitespace splitting, **no quoting support**: an argument that itself
  needs an embedded space can't be expressed in v1 (documented limitation,
  same class of cut as the base feature's own "single program name" was
  before this doc). An all-whitespace or empty field parses to `vec![]`,
  matching "no arguments needed" — not an error case, unlike the
  name/extension/command fields.
- "Languages…" settings window (`render_language_settings_window`): a
  fourth text field ("Arguments") next to Name/Extension/Command; each
  existing entry's row shows `"{name} (.{extension}) — {command}
  {args.join(\" \")}"` (args segment omitted entirely when empty, not
  shown as a trailing space).
- `render_language_suggestion_popup`'s label includes args when present:
  `"{marker_file} detected. Enable {name} language support ({command}
  {args...})?"`.

## 3. Behavior notes

- Two markers can now share a command trivially (`typescript-language-server`
  is the same program either way) — nothing in this feature assumes a
  1:1 command-to-language mapping; that was already true before this
  pass (nothing stopped two different `custom_languages` entries naming
  the same `command` with different `extension`s).
- `detect_language`'s own re-matching (tree-wide extension scan for
  `custom_languages`, unrelated to `detect_language_suggestions`) is
  untouched by this feature — still keyed on `extension` alone, `args`
  plays no role in *whether* a config matches, only in *how* its server
  gets launched once it does.

## 4. Security notes (`crates/lsp/**` and `crates/ui/src/lsp_bridge.rs`
are both unconditionally security-sensitive per `CLAUDE.md` — `hacker`
pass required before merge)

- **Still no shell.** `Command::new(command).args(args)` builds a real
  argv (execve-style), never a shell command line — a stray `;`/`|`/`&&`/
  `$(...)` inside an `args` entry is just literal argv content passed to
  `command`, not shell syntax. This was already true for `command` itself
  (`global-search-and-languages.md` §4); this feature widens the *same*
  guarantee to cover more of the argv, not a different one.
- **The trust boundary is unchanged in kind, only in degree.** Before this
  feature, a `custom_languages` entry already let the user (or a crafted
  `.ide/preferences.json`) make this app spawn any program on `PATH`,
  cwd'd at the project root — `global-search-and-languages.md` §4 already
  accepted that as "a locally self-inflicted action, the same trust
  boundary as the user typing a command into their own terminal." `args`
  extends that to also choosing what gets passed to that program — no new
  *kind* of trust is extended, the same already-accepted actor (whoever
  can edit this project's `custom_languages`, by hand or via a crafted
  preferences file) gets a wider version of a capability it already had.
- **What `hacker` should actually verify, not just assert:** that a
  malformed `args` entry (embedded NUL byte via a hand-crafted
  `preferences.json`, since `String` can hold one even though a real
  terminal/text field can't type one; an empty string as an argument; an
  extremely long single argument; hundreds of arguments) produces a clean
  `LspError`/spawn failure — never a panic — live, not just by reading
  `Command::spawn`'s documented behavior. Also confirm that `args` entries
  never get concatenated into one string and re-split/re-interpreted
  anywhere on the way from `ProjectPreferences` JSON to `Command::args`
  (they must stay a `Vec<String>` end to end) — a concatenate-then-split
  step would be exactly the kind of accidental shell-like reinterpretation
  this design is supposed to avoid.

## 5. Tests (required, per the implementing roles' own skills)

**`ide-core`**: `LanguageConfig` round-trips through `serde_json` with and
without an `"args"` key present (the latter proving the
`#[serde(default)]` migration path). `detect_language_suggestions`:
Python matches on each of its three markers individually and prefers
`pyproject.toml` when more than one is present simultaneously (first-wins
order); TypeScript matches `tsconfig.json` and does **not** match on
`package.json` alone; the returned `args` for Go/Python is empty and for
TypeScript is `["--stdio"]`.

**`ide-lsp`**: `spawn_child`/`start_with_command` — an empty `args` slice
behaves identically to before this change (regression guard); a non-empty
`args` slice is actually passed to the spawned process (e.g. spawn `echo`
or a small test script and confirn its output reflects the argv it
received — the same "observe real process behavior" bar this crate's
existing spawn tests already meet, not just a type-level check); the NUL-
byte/empty-string/many-arguments cases from §4 all produce `LspError`, not
a panic.

**`ide-ui`**: `add_custom_language`'s whitespace-splitting (empty field →
`vec![]`; multiple space-separated tokens → each becomes one `args`
entry; leading/trailing whitespace trimmed); the "Languages…" row label's
args-present vs. args-empty rendering (covered by a plain string-building
test, no `egui` needed, same convention this file already uses for other
label-formatting logic if any exists — else a straightforward new pure
helper); every `start_with_command` call site updated to pass `args`
correctly (existing tests already exercise these call sites and just need
their assertions to keep passing once signatures change).

## Revision notes

Self-review (doc + code, combined, performed inline per this session's
standing "no background agents" instruction):

1. `crates/tui/src/lsp_bridge.rs`/`app.rs` weren't in this doc's original
   scope but broke the build once `ide_lsp::LspClient::start_with_command`'s
   signature changed — `ide-tui` depends on `ide-lsp` directly and has its
   own parallel `LspBridge::start_with_command` wrapper. Fixed by threading
   `args: &[String]` through there too, for signature parity and forward
   compatibility, even though `ide-tui` has no `custom_languages`/
   Languages-settings UI yet (confirmed live: its one call site always
   passes `&[]` today, via `detect_language(&tree, &[])`'s always-empty
   `custom` list) — not a new attack surface, just kept the two frontends'
   `ide-lsp` usage consistent instead of hardcoding `&[]` at that call site
   and letting it silently drift once `ide-tui` does grow its own
   language-settings feature.
2. Widened `LanguageMarker`'s single `marker_file: &'static str` to
   `marker_files: &'static [&'static str]` (OR-matched, first wins) while
   implementing Python's marker — `pyproject.toml`/`setup.py`/
   `requirements.txt` are three real, independently-common signals for
   "this is a Python project," and asking three separate times for the
   same answer would have been actual noise, not just an implementation
   inconvenience. `LanguageSuggestion.marker_file` became an owned
   `String` as a consequence (which one matched is now a runtime fact).
3. Deliberately used `tsconfig.json`, not `package.json`, for the
   TypeScript marker — caught during design, before writing any code,
   that `package.json` exists for any Node project including plain
   JavaScript ones with no TypeScript at all, which would have meant
   suggesting a TypeScript language server on top of a project that never
   asked for one.
4. Hacker pass (required — this diff touches `crates/lsp/**` and
   `crates/ui/src/lsp_bridge.rs`, both `CLAUDE.md`-declared
   security-sensitive): one Low-severity `MetadataLeak` finding (argv,
   including `args`, visible to other local processes via `ps` — not new
   in kind, `command` was already visible the same way, but `args`
   plausibly invites a secret where a program name rarely would) — fixed
   with a hover-text caution on the Arguments field rather than a code
   change (there's no way to hide argv from the OS process table). No
   Medium/High/Critical findings. The core "still no shell" claim was
   proven live: a real spawned `echo` given an argv entry packed with
   shell metacharacters and command substitutions did not execute them
   (confirmed via a canary file that was never created) and echoed the
   payload back verbatim. Full findings:
   `docs/security-findings/rust-lsp-dev-language-server-arguments-2026-08-28.md`.
5. Coverage: `crates/core/src/language.rs` 100%, `crates/lsp/src/client.rs`
   94.45%, `crates/ui/src/lsp_bridge.rs` 90.13%, `crates/ui/src/app.rs`
   96.54%, `crates/tui/src/lsp_bridge.rs` 92.48%, `crates/tui/src/app.rs`
   97.03%. `crates/ui/src/app/render.rs`'s new pure `command_line` helper
   is tested despite render.rs's general pure-rendering exemption (it has
   no `egui` dependency, so there was no reason not to test it); the two
   `egui`-calling edits around it (the popup label, the settings-window
   row/field) stay exempt like the rest of the file.
6. Full workspace `cargo fmt --all -- --check` /
   `cargo clippy --workspace --all-targets -- -D warnings` /
   `cargo build --workspace --all-targets` / `cargo test --workspace`
   all green (2342 tests). One `ide-tui` test,
   `esc_during_capture_cancels_without_assigning_a_binding`, failed once
   under full-workspace-suite load and passed cleanly both in isolation
   and on a full clean re-run immediately after — a pre-existing flaky
   test unrelated to this diff (keymap-capture timing, nothing this diff
   touches), not a regression introduced here.

### 2026-08-28 addendum: expand `LANGUAGE_MARKERS` to every manually-configurable example language

User request: "lets add all of them," in reply to a list of 12
manually-configurable example languages given in answer to "whose langs
are can be added?". `LANGUAGE_MARKERS` (`crates/core/src/language.rs`)
grew from 3 entries (Go/Python/TypeScript) to 14, resolving the two
exclusions §1 originally documented rather than leaving them out:

| Language | Marker(s) | Command | Args |
|---|---|---|---|
| C/C++ | `CMakeLists.txt` | `clangd` | — |
| Java | `pom.xml` | `jdtls` | `-data {project_root}/.jdtls-workspace` |
| Ruby | `Gemfile` | `solargraph` | `stdio` |
| PHP | `composer.json` | `intelephense` | `--stdio` |
| Swift | `Package.swift` | `sourcekit-lsp` | — |
| Kotlin | `build.gradle.kts` | `kotlin-language-server` | — |
| Lua | `.luarc.json` | `lua-language-server` | — |
| Zig | `build.zig` | `zls` | — |
| Haskell | `stack.yaml` or `cabal.project` | `haskell-language-server-wrapper` | `--lsp` |
| Elixir | `mix.exs` | `elixir-ls` | — |
| Dart | `pubspec.yaml` | `dart` | `language-server` |

Design decisions made resolving the tensions flagged above and elsewhere
in this doc, in the same "make the reasonable call, document the
tradeoff" spirit as the rest of this feature:

- **C/C++'s single-extension gap** (§1's original exclusion reason) is
  actually fixed, not just documented and accepted (a same-day follow-up
  to this addendum, per direct user request after the first version of
  this addendum left it as an accepted risk) — `LanguageConfig` gained a
  second field, `extra_extensions: Vec<String>` (`#[serde(default)]`,
  same backward-compatibility shape as `args`), and `detect_language`'s
  tree-wide scan now matches on `extension` **or** any entry in
  `extra_extensions`. C/C++'s marker sets `extension: "cpp"` plus
  `extra_extensions: ["c", "h", "hpp", "cc", "cxx", "hh", "hxx"]`, so a
  CMake project with zero `.cpp` files (pure C) still re-matches after
  the suggestion is enabled. The same mechanism was applied to close the
  smaller, equivalent gaps already latent in three other new entries:
  Kotlin (`kt` + `kts`), Haskell (`hs` + `lhs`), Elixir (`ex` + `exs`).
  The manual "Add custom language" UI is unaffected — it still exposes
  one extension field and always sets `extra_extensions: []`; only
  `LANGUAGE_MARKERS` entries populate it.
- **Java's `-data <path>` requirement** (§1's other original exclusion
  reason) is resolved with a new, small, generically-useful mechanism
  instead of fabricating a fixed default: `LanguageMarker.args` entries
  may contain the literal placeholder `"{project_root}"`, substituted
  with the real matched project's root path inside
  `detect_language_suggestions` (`String::replace`, not a templating
  engine — one fixed placeholder, one substitution site). `jdtls` gets
  `-data <project_root>/.jdtls-workspace`, a real, project-scoped,
  deterministic path — not a guess, and consistent across restarts for
  the same project.
- **Java's marker is `pom.xml` alone, not `build.gradle`/
  `build.gradle.kts`** — Gradle build files aren't Java-specific (Kotlin,
  Groovy, and Scala projects all use Gradle too), the exact ambiguity
  that already ruled out `package.json` for TypeScript. Maven is close
  enough to Java-only in practice to be a safe marker on its own.
- **Kotlin's marker is `build.gradle.kts` specifically** (not plain
  `build.gradle`) — choosing the Kotlin Gradle DSL for a build script is
  a real signal correlated with Kotlin as the target language, unlike the
  generic Groovy-DSL `build.gradle` which says nothing about which JVM
  language it's building. This also keeps Kotlin's marker disjoint from
  Java's `pom.xml` — no project can match both from the same file.
- **Bash was deliberately left out**, despite being in the user's
  original example list — there is no project-root marker file that
  reliably implies "this is a shell-scripting project" the way
  `go.mod`/`Cargo.toml` do; any project can contain `.sh` files without
  being fundamentally one. Inventing a marker here would violate the same
  "don't invent what isn't real" discipline this project applies to
  keybindings. Still addable manually via the Languages… settings UI,
  unchanged.
- **2026-08-28 update — the caveat above is resolved.** All 11 entries
  added in the "add all of them" batch (C/C++, Java, Ruby, PHP, Swift,
  Kotlin, Lua, Zig, Haskell, Elixir, Dart) were independently checked
  against official docs/READMEs and real editor-config precedent (`nvim-
  lspconfig`, `emacs-lsp`, Zed/Helix language docs, Homebrew formulae) as
  the third item of the standing risk-fix pass that followed the multi-
  language-projects feature — not run against a live install in this
  environment (no toolchain for 11 languages is installed here), but no
  longer just "general knowledge," each command/arg shape traced to a
  specific external source. Every one checked out correct, no code
  changes needed: `clangd`/`sourcekit-lsp`/`kotlin-language-server`/
  `lua-language-server`/`zls`/`elixir-ls` genuinely take no required
  arguments and default to stdio; `solargraph stdio`, `intelephense
  --stdio`, `haskell-language-server-wrapper --lsp`, and `dart
  language-server` all match documented/real config examples verbatim;
  `jdtls -data {project_root}/.jdtls-workspace` matches the
  `nvim-jdtls`/`nvim-lspconfig` convention of a mandatory, unique-per-
  project `-data <workspace-dir>` argument. Searches run via `WebSearch`
  this session; no findings doc since nothing needed fixing — this note
  is the record.

Scope check: `crates/core/src/language.rs` (data table, the new
`extra_extensions` field/matching logic, and the `.replace()` call in
`detect_language_suggestions`) plus one ripple into
`crates/ui/src/app.rs` (every existing `LanguageConfig { .. }` struct
literal — production and test — needed a new `extra_extensions` field
added once the struct gained it; no *logic* in `app.rs` changed, only
literal construction) — confirmed via `git diff --name-only`. Neither
file is on `CLAUDE.md`'s security-sensitive path list (`app.rs` isn't;
`lsp_bridge.rs`/`crates/lsp/**` were untouched), so no `hacker` pass this
round. Full workspace build/fmt/clippy/test green (`cargo test
--workspace`, 734/734 in `ide-tui`, one pre-existing flaky test —
`esc_during_capture_cancels_without_assigning_a_binding` — reconfirmed
via isolated + full clean re-runs, unrelated to this diff which touches
no `crates/tui/**` file). 18 new/changed tests in `language.rs` (one or
more per new marker, a Kotlin-vs-plain-Gradle disjointness test, a Java
`{project_root}` substitution test, a Bash-still-not-matched regression
test, an extra-extensions-key-absent deserialization test, and three
tests specifically proving the C/C++ fix: matching on an extra extension
not just the primary one, the suggestion carrying every extra extension,
and the exact worked scenario — a CMake project with only a `.c` file —
re-matching end to end after being enabled). `crates/core/src/language.rs`
100% line coverage, `crates/ui/src/app.rs` 96.54%
(`cargo llvm-cov -p ide-core|ide-ui --summary-only`).
