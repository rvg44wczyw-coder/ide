//! Language detection for the additional-languages feature (see
//! `docs/features/global-search-and-languages.md`). Rust is a permanent,
//! non-configurable special case (a `Cargo.toml`-at-root marker check,
//! not "any `.rs` file exists") so it always wins over any user-added
//! config regardless of `custom`'s order or contents.

use crate::project::{DirEntry, DirEntryKind};
use std::path::Path;

/// Caps `LanguageConfig.args`/`extra_extensions` at parse time -- same
/// early-stopping discipline `ide-lsp`'s `Bounded*` wire-response wrappers
/// already use (`crates/lsp/src/client.rs`'s `BoundedInlayHints` and
/// siblings), applied here because these fields come from the same class
/// of untrusted-file source (a project's `.ide/preferences.json`, read by
/// `ide-ui`'s `ProjectPreferences`) that motivated those wrappers: a
/// `custom_languages` entry with a huge `args` array shouldn't get to
/// allocate proportionally to attacker/corruption-controlled input size
/// before anything downstream ever sees it
/// (`docs/security-findings/rust-ui-dev-multi-language-projects-2026-08-28.md`,
/// finding 1's suggested fix direction).
const MAX_LANGUAGE_CONFIG_LIST_LEN: usize = 256;

fn deserialize_bounded_string_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct BoundedVisitor;

    impl<'de> serde::de::Visitor<'de> for BoundedVisitor {
        type Value = Vec<String>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("an array of strings")
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let mut items = Vec::new();
            while items.len() < MAX_LANGUAGE_CONFIG_LIST_LEN {
                match seq.next_element::<String>()? {
                    Some(item) => items.push(item),
                    None => return Ok(items),
                }
            }
            while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
            Ok(items)
        }
    }

    deserializer.deserialize_seq(BoundedVisitor)
}

/// One language a project can be detected as, and the command to spawn
/// its language server. `command` is a single program name/path, spawned
/// via `Command::new` with no shell -- `args` (`docs/features/
/// language-server-arguments.md`) is a real argv passed to it via
/// `.args()`, never concatenated into a shell string.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LanguageConfig {
    pub name: String,
    /// No leading `.` — e.g. `"go"`, not `".go"`. The *primary* extension
    /// -- shown in the "Languages…" settings row and used as the display
    /// value -- but not the only one `detect_language` matches against;
    /// see `extra_extensions`.
    pub extension: String,
    pub command: String,
    /// Argv entries passed to `command`, in order, after the program name
    /// itself -- e.g. `["--stdio"]`. Empty for a server that runs
    /// correctly with zero arguments (most of them).
    /// `#[serde(default)]` so a `custom_languages` entry persisted by a
    /// build before this field existed (no `"args"` key in its JSON)
    /// still deserializes as `args: vec![]` instead of failing to load.
    #[serde(default, deserialize_with = "deserialize_bounded_string_vec")]
    pub args: Vec<String>,
    /// Additional extensions (no leading `.`) that also count as a match
    /// for this language in `detect_language`'s tree-wide scan, alongside
    /// `extension` -- e.g. C/C++'s `extension: "cpp"` plus
    /// `extra_extensions: ["c", "h", "hpp", "cc", "cxx", "hh", "hxx"]`, so
    /// a C-only or C++-only project (no `.cpp` file at all) still
    /// re-matches after being enabled, not just a mixed-extension one.
    /// `#[serde(default)]` for the same backward-compatibility reason as
    /// `args`. The manual "Add custom language" UI never sets this (it
    /// only exposes one extension field) -- it's populated only by
    /// `LANGUAGE_MARKERS` entries that need it.
    #[serde(default, deserialize_with = "deserialize_bounded_string_vec")]
    pub extra_extensions: Vec<String>,
    /// Debug adapter program, analogous to `command` for the language
    /// server (`docs/features/debugger.md` §2.2). `None`, or a
    /// whitespace-only string, means "no debugger configured for this
    /// language" -- `debug_adapter()` is the one call site that checks.
    /// `#[serde(default)]` for the same backward-compatibility reason as
    /// `args`: a `custom_languages` entry persisted before this field
    /// existed still deserializes, with no debug adapter configured.
    #[serde(default)]
    pub debug_adapter_command: Option<String>,
    /// Argv for `debug_adapter_command`, same bounded-deserialize
    /// treatment as `args`.
    #[serde(default, deserialize_with = "deserialize_bounded_string_vec")]
    pub debug_adapter_args: Vec<String>,
}

impl LanguageConfig {
    /// The one built-in config. Never persisted or exposed as a
    /// `custom_languages` entry — `detect_language` always special-cases
    /// it ahead of `custom`, so it can't be shadowed, removed, or
    /// reordered by whatever the caller's `custom` list contains.
    pub fn rust() -> LanguageConfig {
        LanguageConfig {
            name: "Rust".to_string(),
            extension: "rs".to_string(),
            command: "rust-analyzer".to_string(),
            args: Vec::new(),
            extra_extensions: Vec::new(),
            ..Default::default()
        }
    }

    /// `None` when no debug adapter is configured for this language
    /// (`debug_adapter_command` is `None`, or trims to empty) -- the one
    /// call site `ide-ui` uses to decide whether "Debug" is enabled for
    /// the active file's language (`docs/features/debugger.md` §2.2).
    pub fn debug_adapter(&self) -> Option<(&str, &[String])> {
        let command = self.debug_adapter_command.as_deref()?.trim();
        if command.is_empty() {
            return None;
        }
        Some((command, self.debug_adapter_args.as_slice()))
    }
}

/// One root marker this build knows a default language-server command
/// for, distinct from `LanguageConfig::rust()`'s permanent, non-optional
/// special case above -- these are opt-in suggestions (`docs/features/
/// language-auto-detect.md`, `docs/features/language-server-arguments.md`),
/// not another hardcoded detection path. `command`/`args` must be
/// verified to actually run correctly over stdio before an entry is added
/// here -- see those docs' §1 for why the list stays short.
struct LanguageMarker {
    /// No path separators -- each checked as a direct child of the
    /// project root only, exactly like the `Cargo.toml` check above,
    /// never a tree-wide scan. Tried in order; the first one present
    /// wins (a project only needs one "yes, this is $language" answer,
    /// not one prompt per possible marker).
    marker_files: &'static [&'static str],
    name: &'static str,
    extension: &'static str,
    command: &'static str,
    /// May contain the literal placeholder `"{project_root}"`, substituted
    /// with the matched project's actual root path in
    /// [`detect_language_suggestions`] -- needed for servers (`jdtls`)
    /// whose required argument is a real per-project path with no
    /// universal fixed value (see the `Java` entry below).
    args: &'static [&'static str],
    /// Copied into [`LanguageConfig::extra_extensions`] -- other
    /// extensions that should also count as a match for this language
    /// besides `extension`. Empty for languages with one real source
    /// extension.
    extra_extensions: &'static [&'static str],
}

const LANGUAGE_MARKERS: &[LanguageMarker] = &[
    LanguageMarker {
        marker_files: &["go.mod"],
        name: "Go",
        extension: "go",
        command: "gopls",
        args: &[],
        extra_extensions: &[],
    },
    LanguageMarker {
        marker_files: &["pyproject.toml", "setup.py", "requirements.txt"],
        name: "Python",
        extension: "py",
        command: "pylsp",
        args: &[],
        // `.pyi` -- type-stub files, an official part of the Python
        // typing ecosystem (PEP 484) -- so a stub-only package (no `.py`
        // file at all) still re-matches after being enabled, the same
        // single-extension gap the C/C++/Kotlin/Haskell/Elixir markers
        // already closed for their own languages.
        extra_extensions: &["pyi"],
    },
    LanguageMarker {
        // Deliberately not `package.json`: that file exists for any
        // Node project, including plain JavaScript ones with no
        // TypeScript at all. `tsconfig.json` is the marker that
        // actually implies TypeScript.
        marker_files: &["tsconfig.json"],
        name: "TypeScript",
        extension: "ts",
        command: "typescript-language-server",
        args: &["--stdio"],
        // `.tsx` -- TypeScript's own JSX source extension, not a
        // separate language -- so a React/TSX-only project (no plain
        // `.ts` file at all) still re-matches after being enabled.
        extra_extensions: &["tsx"],
    },
    LanguageMarker {
        // A single extension can't represent a C/C++ project on its own
        // -- fixed (not just documented) by giving `extension`/
        // `extra_extensions` every common C/C++ source/header extension,
        // so a CMake project re-matches `detect_language`'s tree-wide
        // scan regardless of which of these it actually uses.
        marker_files: &["CMakeLists.txt"],
        name: "C/C++",
        extension: "cpp",
        command: "clangd",
        args: &[],
        extra_extensions: &["c", "h", "hpp", "cc", "cxx", "hh", "hxx"],
    },
    LanguageMarker {
        // `pom.xml` only, not `build.gradle`/`build.gradle.kts` --
        // Gradle build files aren't Java-specific (Kotlin/Groovy/Scala
        // all use Gradle too), the same ambiguity that rules out
        // `package.json` for TypeScript above. Maven is close enough to
        // Java-only in practice to be a safe marker.
        marker_files: &["pom.xml"],
        name: "Java",
        extension: "java",
        command: "jdtls",
        // jdtls requires a per-project `-data` workspace directory with
        // no sane universal default; `{project_root}` is substituted
        // with the real matched root at detection time.
        args: &["-data", "{project_root}/.jdtls-workspace"],
        extra_extensions: &[],
    },
    LanguageMarker {
        marker_files: &["Gemfile"],
        name: "Ruby",
        extension: "rb",
        command: "solargraph",
        args: &["stdio"],
        extra_extensions: &[],
    },
    LanguageMarker {
        marker_files: &["composer.json"],
        name: "PHP",
        extension: "php",
        command: "intelephense",
        args: &["--stdio"],
        extra_extensions: &[],
    },
    LanguageMarker {
        marker_files: &["Package.swift"],
        name: "Swift",
        extension: "swift",
        command: "sourcekit-lsp",
        args: &[],
        extra_extensions: &[],
    },
    LanguageMarker {
        // Kotlin's Gradle DSL file, not plain `build.gradle` (see the
        // Java entry above) -- choosing the Kotlin DSL for the build
        // script is a much stronger Kotlin signal than a generic Gradle
        // marker would be.
        marker_files: &["build.gradle.kts"],
        name: "Kotlin",
        extension: "kt",
        command: "kotlin-language-server",
        args: &[],
        // Kotlin script files -- a project with only .kts build/script
        // files and no .kt sources yet still re-matches.
        extra_extensions: &["kts"],
    },
    LanguageMarker {
        // `lua-language-server`'s own config file -- unambiguous, unlike
        // any single Lua source-file convention.
        marker_files: &[".luarc.json"],
        name: "Lua",
        extension: "lua",
        command: "lua-language-server",
        args: &[],
        extra_extensions: &[],
    },
    LanguageMarker {
        marker_files: &["build.zig"],
        name: "Zig",
        extension: "zig",
        command: "zls",
        args: &[],
        extra_extensions: &[],
    },
    LanguageMarker {
        marker_files: &["stack.yaml", "cabal.project"],
        name: "Haskell",
        extension: "hs",
        command: "haskell-language-server-wrapper",
        args: &["--lsp"],
        // Literate Haskell.
        extra_extensions: &["lhs"],
    },
    LanguageMarker {
        marker_files: &["mix.exs"],
        name: "Elixir",
        extension: "ex",
        command: "elixir-ls",
        args: &[],
        // Elixir script files (e.g. `mix.exs` itself) -- a project with
        // only `.exs` files still re-matches.
        extra_extensions: &["exs"],
    },
    LanguageMarker {
        marker_files: &["pubspec.yaml"],
        name: "Dart",
        extension: "dart",
        command: "dart",
        args: &["language-server"],
        extra_extensions: &[],
    },
    // Bash deliberately has no entry: there is no project-root marker
    // file that reliably implies "this is a shell-scripting project" the
    // way `go.mod`/`Cargo.toml` do -- any project can contain `.sh`
    // files without being one. Still addable manually via the Languages…
    // settings UI.
];

/// One "`marker_file` exists at the project root -- want to add this
/// config?" suggestion, returned by [`detect_language_suggestions`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageSuggestion {
    /// The specific marker file that actually matched (e.g. `"setup.py"`
    /// when a project has that but not `pyproject.toml`) -- a runtime
    /// fact, not a fixed table value, since a marker can have more than
    /// one candidate filename.
    pub marker_file: String,
    pub config: LanguageConfig,
}

/// Every [`LANGUAGE_MARKERS`] entry with at least one `marker_files`
/// candidate present directly under `project_root`, in table order. Pure
/// detection: doesn't consult `custom_languages`, doesn't check whether
/// Rust already claimed the project, and doesn't persist or dismiss
/// anything -- the caller decides what to do with the result
/// (`docs/features/language-auto-detect.md` §2.1/§4).
pub fn detect_language_suggestions(project_root: &Path) -> Vec<LanguageSuggestion> {
    LANGUAGE_MARKERS
        .iter()
        .filter_map(|m| {
            let matched = m
                .marker_files
                .iter()
                .find(|f| project_root.join(f).exists())?;
            let root = project_root.display().to_string();
            Some(LanguageSuggestion {
                marker_file: matched.to_string(),
                config: LanguageConfig {
                    name: m.name.to_string(),
                    extension: m.extension.to_string(),
                    command: m.command.to_string(),
                    args: m
                        .args
                        .iter()
                        .map(|s| s.replace("{project_root}", &root))
                        .collect(),
                    extra_extensions: m.extra_extensions.iter().map(|s| s.to_string()).collect(),
                    ..Default::default()
                },
            })
        })
        .collect()
}

/// Detects which language applies to `tree`'s project. Rust is checked
/// first: matches only when `tree.path` (the project root — see
/// `Project::scan_tree`) has a `Cargo.toml` directly in it. Every
/// `custom` entry after that matches if any file anywhere in `tree` has
/// `extension` **or** any of `extra_extensions` (case-insensitive),
/// checked in `custom`'s order, first match wins. `None` if nothing
/// matches.
pub fn detect_language(tree: &DirEntry, custom: &[LanguageConfig]) -> Option<LanguageConfig> {
    if tree.path.join("Cargo.toml").exists() {
        return Some(LanguageConfig::rust());
    }
    custom
        .iter()
        .find(|config| config_matches_tree(config, tree))
        .cloned()
}

/// A project with more than this many simultaneously matching languages
/// gets truncated, not an unbounded number of spawned LSP subprocesses
/// (`docs/features/multi-language-projects.md` §2.1/§4).
const MAX_ACTIVE_LANGUAGES: usize = 8;

fn config_matches_tree(config: &LanguageConfig, tree: &DirEntry) -> bool {
    tree_has_extension(tree, &config.extension)
        || config
            .extra_extensions
            .iter()
            .any(|ext| tree_has_extension(tree, ext))
}

/// Every language active for `tree`'s project: Rust first (if
/// `Cargo.toml` is at the root) -- no longer exclusive, just first --
/// followed by every `custom` entry (in `custom`'s order) whose
/// `extension` or any `extra_extensions` entry matches a file anywhere in
/// `tree`. A `custom` entry whose `extension` case-insensitively equals
/// `"rs"` is skipped (Rust's slot can't be shadowed or duplicated) and
/// entries are deduplicated by `extension` (case-insensitive) -- but only
/// among entries that actually match: a non-matching earlier entry
/// sharing an extension with a later matching one never blocks the later
/// one, since it never claims the extension itself. Truncated to
/// [`MAX_ACTIVE_LANGUAGES`] -- Rust, when matched, is always considered
/// first and so is never the entry dropped by truncation
/// (`docs/features/multi-language-projects.md` §2.1).
pub fn detect_active_languages(tree: &DirEntry, custom: &[LanguageConfig]) -> Vec<LanguageConfig> {
    let mut result = Vec::new();
    let mut claimed = std::collections::HashSet::new();
    if tree.path.join("Cargo.toml").exists() {
        result.push(LanguageConfig::rust());
        claimed.insert("rs".to_string());
    }
    for config in custom {
        if result.len() >= MAX_ACTIVE_LANGUAGES {
            break;
        }
        let extension = config.extension.to_lowercase();
        if extension == "rs" || claimed.contains(&extension) {
            continue;
        }
        if config_matches_tree(config, tree) {
            claimed.insert(extension);
            result.push(config.clone());
        }
    }
    result
}

/// The first entry in `active` whose `extension` or any `extra_extensions`
/// entry matches `path`'s extension (case-insensitive), or `None` if
/// nothing in `active` covers it -- the per-file routing primitive
/// `LspBridge` uses to pick which running client answers a request about a
/// given path (`docs/features/multi-language-projects.md` §2.1).
pub fn language_for_path<'a>(
    active: &'a [LanguageConfig],
    path: &Path,
) -> Option<&'a LanguageConfig> {
    let extension = path.extension()?.to_str()?;
    active.iter().find(|config| {
        config.extension.eq_ignore_ascii_case(extension)
            || config
                .extra_extensions
                .iter()
                .any(|e| e.eq_ignore_ascii_case(extension))
    })
}

fn tree_has_extension(entry: &DirEntry, extension: &str) -> bool {
    match entry.kind {
        DirEntryKind::File => entry
            .path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case(extension)),
        DirEntryKind::Dir => entry
            .children
            .iter()
            .any(|child| tree_has_extension(child, extension)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::Project;
    use std::fs;

    fn go_config() -> LanguageConfig {
        LanguageConfig {
            name: "Go".to_string(),
            extension: "go".to_string(),
            command: "gopls".to_string(),
            args: Vec::new(),
            extra_extensions: Vec::new(),
            ..Default::default()
        }
    }

    #[test]
    fn args_deserializes_normally_under_the_cap() {
        let config: LanguageConfig = serde_json::from_str(
            r#"{"name":"Go","extension":"go","command":"gopls","args":["--stdio","-v"]}"#,
        )
        .unwrap();
        assert_eq!(config.args, vec!["--stdio".to_string(), "-v".to_string()]);
    }

    #[test]
    fn args_missing_key_defaults_to_empty() {
        let config: LanguageConfig =
            serde_json::from_str(r#"{"name":"Go","extension":"go","command":"gopls"}"#).unwrap();
        assert!(config.args.is_empty());
        assert!(config.extra_extensions.is_empty());
    }

    #[test]
    fn language_config_deserializes_with_no_debug_adapter_keys_present() {
        // Old-shape JSON, persisted before F5a's fields existed at all.
        let json = r#"{"name":"Go","extension":"go","command":"gopls"}"#;
        let config: LanguageConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.debug_adapter_command, None);
        assert!(config.debug_adapter_args.is_empty());
        assert_eq!(config.debug_adapter(), None);
    }

    #[test]
    fn language_config_round_trips_with_debug_adapter() {
        let config = LanguageConfig {
            name: "Rust".to_string(),
            extension: "rs".to_string(),
            command: "rust-analyzer".to_string(),
            debug_adapter_command: Some("codelldb".to_string()),
            debug_adapter_args: vec!["--port".to_string(), "0".to_string()],
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let reloaded: LanguageConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(reloaded, config);
    }

    #[test]
    fn debug_adapter_is_none_when_no_command_is_configured() {
        let config = go_config();
        assert_eq!(config.debug_adapter(), None);
    }

    #[test]
    fn debug_adapter_is_none_when_command_is_only_whitespace() {
        let config = LanguageConfig {
            debug_adapter_command: Some("   ".to_string()),
            ..go_config()
        };
        assert_eq!(config.debug_adapter(), None);
    }

    #[test]
    fn debug_adapter_returns_trimmed_command_and_args() {
        let config = LanguageConfig {
            debug_adapter_command: Some("  codelldb  ".to_string()),
            debug_adapter_args: vec!["--port".to_string(), "0".to_string()],
            ..go_config()
        };
        assert_eq!(
            config.debug_adapter(),
            Some((
                "codelldb",
                ["--port".to_string(), "0".to_string()].as_slice()
            ))
        );
    }

    #[test]
    fn args_beyond_the_cap_truncates_instead_of_allocating_unbounded() {
        let huge: Vec<String> = (0..(MAX_LANGUAGE_CONFIG_LIST_LEN + 500))
            .map(|i| i.to_string())
            .collect();
        let json = serde_json::json!({
            "name": "Go",
            "extension": "go",
            "command": "gopls",
            "args": huge,
        });
        let config: LanguageConfig = serde_json::from_value(json).unwrap();
        assert_eq!(config.args.len(), MAX_LANGUAGE_CONFIG_LIST_LEN);
        // Truncation keeps the *first* N entries, not an arbitrary subset.
        assert_eq!(config.args[0], "0");
        assert_eq!(
            config.args[MAX_LANGUAGE_CONFIG_LIST_LEN - 1],
            (MAX_LANGUAGE_CONFIG_LIST_LEN - 1).to_string()
        );
    }

    #[test]
    fn extra_extensions_beyond_the_cap_truncates_and_parsing_still_succeeds() {
        let huge: Vec<String> = (0..(MAX_LANGUAGE_CONFIG_LIST_LEN * 3))
            .map(|i| format!("ext{i}"))
            .collect();
        let json = serde_json::json!({
            "name": "C",
            "extension": "cpp",
            "command": "clangd",
            "extra_extensions": huge,
        });
        let config: LanguageConfig = serde_json::from_value(json).unwrap();
        assert_eq!(config.extra_extensions.len(), MAX_LANGUAGE_CONFIG_LIST_LEN);
    }

    #[test]
    fn args_rejects_a_non_array_value_instead_of_silently_defaulting() {
        let result: Result<LanguageConfig, _> = serde_json::from_str(
            r#"{"name":"Go","extension":"go","command":"gopls","args":"not-an-array"}"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn detects_rust_via_cargo_toml_marker_regardless_of_rs_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        // No .rs files at all -- the marker alone is sufficient.
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let detected = detect_language(&tree, &[]).unwrap();
        assert_eq!(detected, LanguageConfig::rust());
    }

    #[test]
    fn rust_marker_wins_over_a_custom_config_even_when_listed_first() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        fs::write(dir.path().join("main.go"), "package main").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let detected = detect_language(&tree, &[go_config()]).unwrap();
        assert_eq!(detected, LanguageConfig::rust());
    }

    #[test]
    fn detects_custom_language_by_extension_anywhere_in_the_tree() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("cmd")).unwrap();
        fs::write(dir.path().join("cmd/main.go"), "package main").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let go = go_config();
        let detected = detect_language(&tree, std::slice::from_ref(&go)).unwrap();
        assert_eq!(detected, go);
    }

    #[test]
    fn extension_matching_is_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("main.GO"), "package main").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        assert!(detect_language(&tree, &[go_config()]).is_some());
    }

    #[test]
    fn first_matching_custom_config_wins() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("main.go"), "package main").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let go_a = LanguageConfig {
            name: "Go A".to_string(),
            extension: "go".to_string(),
            command: "gopls-a".to_string(),
            args: Vec::new(),
            extra_extensions: Vec::new(),
            ..Default::default()
        };
        let go_b = LanguageConfig {
            name: "Go B".to_string(),
            extension: "go".to_string(),
            command: "gopls-b".to_string(),
            args: Vec::new(),
            extra_extensions: Vec::new(),
            ..Default::default()
        };
        let detected = detect_language(&tree, &[go_a.clone(), go_b]).unwrap();
        assert_eq!(detected, go_a);
    }

    #[test]
    fn detect_language_suggestions_matches_go_mod_at_root() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("go.mod"), "module example.com/foo").unwrap();

        let suggestions = detect_language_suggestions(dir.path());
        assert_eq!(
            suggestions,
            vec![LanguageSuggestion {
                marker_file: "go.mod".to_string(),
                config: go_config(),
            }]
        );
    }

    #[test]
    fn detect_language_suggestions_ignores_a_nested_go_mod() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/go.mod"), "module example.com/foo").unwrap();

        assert!(detect_language_suggestions(dir.path()).is_empty());
    }

    #[test]
    fn detect_language_suggestions_returns_empty_with_no_recognized_marker() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("readme.txt"), "hi").unwrap();

        assert!(detect_language_suggestions(dir.path()).is_empty());
    }

    #[test]
    fn detect_language_suggestions_matches_python_on_each_marker_individually() {
        for marker in ["pyproject.toml", "setup.py", "requirements.txt"] {
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join(marker), "").unwrap();

            let suggestions = detect_language_suggestions(dir.path());
            assert_eq!(suggestions.len(), 1, "marker {marker}");
            assert_eq!(suggestions[0].marker_file, marker);
            assert_eq!(suggestions[0].config.name, "Python");
            assert_eq!(suggestions[0].config.command, "pylsp");
            assert!(suggestions[0].config.args.is_empty());
        }
    }

    #[test]
    fn detect_language_suggestions_prefers_pyproject_toml_when_multiple_python_markers_exist() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("pyproject.toml"), "").unwrap();
        fs::write(dir.path().join("setup.py"), "").unwrap();

        let suggestions = detect_language_suggestions(dir.path());
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].marker_file, "pyproject.toml");
    }

    #[test]
    fn detect_language_suggestions_matches_typescript_on_tsconfig_with_stdio_arg() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("tsconfig.json"), "{}").unwrap();

        let suggestions = detect_language_suggestions(dir.path());
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].config.name, "TypeScript");
        assert_eq!(suggestions[0].config.command, "typescript-language-server");
        assert_eq!(suggestions[0].config.args, vec!["--stdio".to_string()]);
    }

    #[test]
    fn detect_language_suggestions_does_not_match_typescript_on_package_json_alone() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("package.json"), "{}").unwrap();

        assert!(detect_language_suggestions(dir.path()).is_empty());
    }

    #[test]
    fn detect_language_suggestions_can_return_more_than_one_marker_at_once() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("go.mod"), "module example.com/foo").unwrap();
        fs::write(dir.path().join("tsconfig.json"), "{}").unwrap();

        let suggestions = detect_language_suggestions(dir.path());
        assert_eq!(suggestions.len(), 2);
    }

    #[test]
    fn detect_language_suggestions_matches_c_cpp_on_cmakelists() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("CMakeLists.txt"), "").unwrap();

        let suggestions = detect_language_suggestions(dir.path());
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].config.name, "C/C++");
        assert_eq!(suggestions[0].config.command, "clangd");
        assert!(suggestions[0].config.args.is_empty());
    }

    #[test]
    fn detect_language_suggestions_matches_java_and_substitutes_project_root_into_data_arg() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("pom.xml"), "").unwrap();

        let suggestions = detect_language_suggestions(dir.path());
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].config.name, "Java");
        assert_eq!(suggestions[0].config.command, "jdtls");
        let expected_data_dir = format!("{}/.jdtls-workspace", dir.path().display());
        assert_eq!(
            suggestions[0].config.args,
            vec!["-data".to_string(), expected_data_dir]
        );
    }

    #[test]
    fn detect_language_suggestions_matches_ruby_on_gemfile() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Gemfile"), "").unwrap();

        let suggestions = detect_language_suggestions(dir.path());
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].config.name, "Ruby");
        assert_eq!(suggestions[0].config.command, "solargraph");
        assert_eq!(suggestions[0].config.args, vec!["stdio".to_string()]);
    }

    #[test]
    fn detect_language_suggestions_matches_php_on_composer_json() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("composer.json"), "{}").unwrap();

        let suggestions = detect_language_suggestions(dir.path());
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].config.name, "PHP");
        assert_eq!(suggestions[0].config.command, "intelephense");
        assert_eq!(suggestions[0].config.args, vec!["--stdio".to_string()]);
    }

    #[test]
    fn detect_language_suggestions_matches_swift_on_package_swift() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Package.swift"), "").unwrap();

        let suggestions = detect_language_suggestions(dir.path());
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].config.name, "Swift");
        assert_eq!(suggestions[0].config.command, "sourcekit-lsp");
    }

    #[test]
    fn detect_language_suggestions_matches_kotlin_on_build_gradle_kts_but_not_plain_gradle() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("build.gradle.kts"), "").unwrap();

        let suggestions = detect_language_suggestions(dir.path());
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].config.name, "Kotlin");
        assert_eq!(suggestions[0].config.command, "kotlin-language-server");

        let dir2 = tempfile::tempdir().unwrap();
        fs::write(dir2.path().join("build.gradle"), "").unwrap();
        assert!(detect_language_suggestions(dir2.path()).is_empty());
    }

    #[test]
    fn detect_language_suggestions_matches_lua_on_luarc_json() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".luarc.json"), "{}").unwrap();

        let suggestions = detect_language_suggestions(dir.path());
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].config.name, "Lua");
        assert_eq!(suggestions[0].config.command, "lua-language-server");
    }

    #[test]
    fn detect_language_suggestions_matches_zig_on_build_zig() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("build.zig"), "").unwrap();

        let suggestions = detect_language_suggestions(dir.path());
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].config.name, "Zig");
        assert_eq!(suggestions[0].config.command, "zls");
    }

    #[test]
    fn detect_language_suggestions_matches_haskell_on_each_marker_individually() {
        for marker in ["stack.yaml", "cabal.project"] {
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join(marker), "").unwrap();

            let suggestions = detect_language_suggestions(dir.path());
            assert_eq!(suggestions.len(), 1, "marker {marker}");
            assert_eq!(suggestions[0].marker_file, marker);
            assert_eq!(suggestions[0].config.name, "Haskell");
            assert_eq!(
                suggestions[0].config.command,
                "haskell-language-server-wrapper"
            );
            assert_eq!(suggestions[0].config.args, vec!["--lsp".to_string()]);
        }
    }

    #[test]
    fn detect_language_suggestions_matches_elixir_on_mix_exs() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("mix.exs"), "").unwrap();

        let suggestions = detect_language_suggestions(dir.path());
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].config.name, "Elixir");
        assert_eq!(suggestions[0].config.command, "elixir-ls");
    }

    #[test]
    fn detect_language_suggestions_matches_dart_on_pubspec_yaml() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("pubspec.yaml"), "").unwrap();

        let suggestions = detect_language_suggestions(dir.path());
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].config.name, "Dart");
        assert_eq!(suggestions[0].config.command, "dart");
        assert_eq!(
            suggestions[0].config.args,
            vec!["language-server".to_string()]
        );
    }

    #[test]
    fn detect_language_suggestions_does_not_match_bash_by_any_marker_since_none_is_registered() {
        // No project-root marker file reliably implies "this is a shell
        // project" -- confirm a directory full of plausible-looking but
        // non-matching files still yields nothing.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("script.sh"), "#!/bin/sh").unwrap();

        assert!(detect_language_suggestions(dir.path()).is_empty());
    }

    #[test]
    fn language_config_deserializes_with_no_args_key_present() {
        let json = r#"{"name":"Go","extension":"go","command":"gopls"}"#;
        let config: LanguageConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config, go_config());
    }

    #[test]
    fn language_config_round_trips_with_args() {
        let config = LanguageConfig {
            name: "TypeScript".to_string(),
            extension: "ts".to_string(),
            command: "typescript-language-server".to_string(),
            args: vec!["--stdio".to_string()],
            extra_extensions: Vec::new(),
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let reloaded: LanguageConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(reloaded, config);
    }

    #[test]
    fn language_config_deserializes_with_no_extra_extensions_key_present() {
        let json = r#"{"name":"Go","extension":"go","command":"gopls","args":[]}"#;
        let config: LanguageConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config, go_config());
    }

    #[test]
    fn detect_language_matches_on_an_extra_extension_not_just_the_primary_one() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("main.c"), "int main(void) {}").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let cpp = LanguageConfig {
            name: "C/C++".to_string(),
            extension: "cpp".to_string(),
            command: "clangd".to_string(),
            args: Vec::new(),
            extra_extensions: vec!["c".to_string(), "h".to_string()],
            ..Default::default()
        };
        let detected = detect_language(&tree, std::slice::from_ref(&cpp)).unwrap();
        assert_eq!(detected, cpp);
    }

    #[test]
    fn detect_language_suggestions_matches_c_cpp_and_carries_every_extra_extension() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("CMakeLists.txt"), "").unwrap();

        let suggestions = detect_language_suggestions(dir.path());
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].config.extension, "cpp");
        assert_eq!(
            suggestions[0].config.extra_extensions,
            vec!["c", "h", "hpp", "cc", "cxx", "hh", "hxx"]
        );
    }

    #[test]
    fn c_only_project_still_activates_after_enabling_the_c_cpp_suggestion() {
        // The exact gap the C/C++ marker's extra_extensions field exists
        // to close: a pure-C CMake project (no .cpp file anywhere) must
        // still re-match `detect_language` once the suggestion is
        // enabled, not just at the moment it's offered.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("CMakeLists.txt"), "").unwrap();
        fs::write(dir.path().join("main.c"), "int main(void) {}").unwrap();

        let suggestions = detect_language_suggestions(dir.path());
        assert_eq!(suggestions.len(), 1);
        let config = suggestions[0].config.clone();

        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();
        assert_eq!(
            detect_language(&tree, std::slice::from_ref(&config)),
            Some(config)
        );
    }

    #[test]
    fn detect_language_suggestions_matches_python_and_carries_the_pyi_extra_extension() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("pyproject.toml"), "").unwrap();

        let suggestions = detect_language_suggestions(dir.path());
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].config.extension, "py");
        assert_eq!(suggestions[0].config.extra_extensions, vec!["pyi"]);
    }

    #[test]
    fn pyi_only_project_still_activates_after_enabling_the_python_suggestion() {
        // The gap `extra_extensions` closes for Python: a stub-only
        // package (no `.py` file anywhere) must still re-match
        // `detect_language` once the suggestion is enabled, not just at
        // the moment it's offered -- same shape as the C/C++ case above.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("pyproject.toml"), "").unwrap();
        fs::write(dir.path().join("stub.pyi"), "def f() -> int: ...").unwrap();

        let suggestions = detect_language_suggestions(dir.path());
        assert_eq!(suggestions.len(), 1);
        let config = suggestions[0].config.clone();

        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();
        assert_eq!(
            detect_language(&tree, std::slice::from_ref(&config)),
            Some(config)
        );
    }

    #[test]
    fn detect_language_suggestions_matches_typescript_and_carries_the_tsx_extra_extension() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("tsconfig.json"), "{}").unwrap();

        let suggestions = detect_language_suggestions(dir.path());
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].config.extension, "ts");
        assert_eq!(suggestions[0].config.extra_extensions, vec!["tsx"]);
    }

    #[test]
    fn tsx_only_project_still_activates_after_enabling_the_typescript_suggestion() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("tsconfig.json"), "{}").unwrap();
        fs::write(
            dir.path().join("App.tsx"),
            "export default function App() {}",
        )
        .unwrap();

        let suggestions = detect_language_suggestions(dir.path());
        assert_eq!(suggestions.len(), 1);
        let config = suggestions[0].config.clone();

        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();
        assert_eq!(
            detect_language(&tree, std::slice::from_ref(&config)),
            Some(config)
        );
    }

    #[test]
    fn no_match_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("readme.txt"), "hi").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        assert_eq!(detect_language(&tree, &[]), None);
        assert_eq!(detect_language(&tree, &[go_config()]), None);
    }

    fn swift_config() -> LanguageConfig {
        LanguageConfig {
            name: "Swift".to_string(),
            extension: "swift".to_string(),
            command: "sourcekit-lsp".to_string(),
            args: Vec::new(),
            extra_extensions: Vec::new(),
            ..Default::default()
        }
    }

    fn kotlin_config() -> LanguageConfig {
        LanguageConfig {
            name: "Kotlin".to_string(),
            extension: "kt".to_string(),
            command: "kotlin-language-server".to_string(),
            args: Vec::new(),
            extra_extensions: vec!["kts".to_string()],
            ..Default::default()
        }
    }

    #[test]
    fn detect_active_languages_with_no_project_and_no_custom_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        assert_eq!(detect_active_languages(&tree, &[]), Vec::new());
    }

    #[test]
    fn detect_active_languages_rust_only_returns_just_rust() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        assert_eq!(
            detect_active_languages(&tree, &[go_config()]),
            vec![LanguageConfig::rust()]
        );
    }

    #[test]
    fn detect_active_languages_combines_rust_and_matching_custom_entries_in_order() {
        // The motivating polyglot worked example (doc §3.1): Rust core
        // plus Swift and Kotlin, all in one tree.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        fs::create_dir(dir.path().join("bindings")).unwrap();
        fs::write(dir.path().join("bindings/lib.swift"), "").unwrap();
        fs::create_dir(dir.path().join("android")).unwrap();
        fs::write(dir.path().join("android/Main.kt"), "").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        assert_eq!(
            detect_active_languages(&tree, &[swift_config(), kotlin_config()]),
            vec![LanguageConfig::rust(), swift_config(), kotlin_config()]
        );
    }

    #[test]
    fn detect_active_languages_excludes_a_custom_entry_with_no_matching_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        assert_eq!(
            detect_active_languages(&tree, &[swift_config()]),
            vec![LanguageConfig::rust()]
        );
    }

    #[test]
    fn detect_active_languages_defensively_excludes_a_custom_entry_claiming_rs() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let fake_rust = LanguageConfig {
            name: "Not Actually Rust".to_string(),
            extension: "rs".to_string(),
            command: "evil-rust-analyzer".to_string(),
            args: Vec::new(),
            extra_extensions: Vec::new(),
            ..Default::default()
        };
        assert_eq!(
            detect_active_languages(&tree, &[fake_rust]),
            vec![LanguageConfig::rust()]
        );
    }

    #[test]
    fn detect_active_languages_keeps_only_the_first_matching_entry_sharing_an_extension() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("main.go"), "package main").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let go_b = LanguageConfig {
            name: "Go B".to_string(),
            extension: "go".to_string(),
            command: "gopls-b".to_string(),
            args: Vec::new(),
            extra_extensions: Vec::new(),
            ..Default::default()
        };
        assert_eq!(
            detect_active_languages(&tree, &[go_config(), go_b]),
            vec![go_config()]
        );
    }

    #[test]
    fn detect_active_languages_truncates_to_the_cap_without_dropping_rust() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        let mut custom = Vec::new();
        for i in 0..(MAX_ACTIVE_LANGUAGES + 5) {
            let ext = format!("ext{i}");
            fs::write(dir.path().join(format!("f.{ext}")), "").unwrap();
            custom.push(LanguageConfig {
                name: format!("Lang {i}"),
                extension: ext,
                command: "some-lsp".to_string(),
                args: Vec::new(),
                extra_extensions: Vec::new(),
                ..Default::default()
            });
        }
        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let active = detect_active_languages(&tree, &custom);
        assert_eq!(active.len(), MAX_ACTIVE_LANGUAGES);
        assert_eq!(active[0], LanguageConfig::rust());
    }

    #[test]
    fn language_for_path_matches_primary_extension_case_insensitively() {
        let active = vec![go_config()];
        assert_eq!(
            language_for_path(&active, Path::new("/x/main.GO")),
            Some(&active[0])
        );
    }

    #[test]
    fn language_for_path_matches_an_extra_extension() {
        let cpp = LanguageConfig {
            name: "C/C++".to_string(),
            extension: "cpp".to_string(),
            command: "clangd".to_string(),
            args: Vec::new(),
            extra_extensions: vec!["c".to_string(), "h".to_string()],
            ..Default::default()
        };
        let active = vec![cpp];
        assert_eq!(
            language_for_path(&active, Path::new("/x/main.c")),
            Some(&active[0])
        );
    }

    #[test]
    fn language_for_path_returns_none_for_an_uncovered_extension() {
        let active = vec![go_config()];
        assert_eq!(language_for_path(&active, Path::new("/x/main.py")), None);
    }

    #[test]
    fn language_for_path_returns_none_for_a_path_with_no_extension() {
        let active = vec![go_config()];
        assert_eq!(language_for_path(&active, Path::new("/x/Makefile")), None);
    }
}
