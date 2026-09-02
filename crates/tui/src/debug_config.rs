//! Global, per-user persisted debug-adapter overrides, keyed by
//! `LanguageConfig::name` (e.g. `"Rust"`) -- `docs/features/
//! tui-debugger.md` §2.1. `ide-tui` has no language-settings UI/
//! persistence at all (unlike `ide-ui`'s per-project `.ide/
//! preferences.json`), so this is the only place a debug adapter command
//! can come from for this frontend. Same "global JSON file at a per-user
//! config path" shape `state.rs`/`keymap.rs` already established.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// One language's debug adapter override.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DebugAdapterEntry {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DebugAdapterConfig {
    pub adapters: HashMap<String, DebugAdapterEntry>,
}

/// Best-effort load, identical contract to `state::load`: a missing file,
/// malformed JSON, or an unresolvable home directory all yield
/// [`DebugAdapterConfig::default`] rather than an error.
pub fn load() -> DebugAdapterConfig {
    match config_file_path() {
        Some(path) => load_from(&path),
        None => DebugAdapterConfig::default(),
    }
}

/// Best-effort save, identical contract to `state::save`: any failure
/// (permission denied, read-only filesystem, no resolvable home
/// directory) is silently swallowed.
pub fn save(config: &DebugAdapterConfig) {
    if let Some(path) = config_file_path() {
        save_to(&path, config);
    }
}

fn load_from(path: &Path) -> DebugAdapterConfig {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default()
}

fn save_to(path: &Path, config: &DebugAdapterConfig) {
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    if let Ok(json) = serde_json::to_string_pretty(config) {
        let _ = std::fs::write(path, json);
    }
}

fn config_file_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(config_file_path_from_home(Path::new(&home)))
}

fn config_file_path_from_home(home: &Path) -> PathBuf {
    home.join(".config/ide-tui/debug_adapters.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_from_a_fresh_directory_returns_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let config = load_from(&dir.path().join("debug_adapters.json"));
        assert_eq!(config, DebugAdapterConfig::default());
    }

    #[test]
    fn save_then_load_round_trips_a_remembered_adapter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/debug_adapters.json");
        let mut remembered = DebugAdapterConfig::default();
        remembered.adapters.insert(
            "Rust".to_string(),
            DebugAdapterEntry {
                command: "codelldb".to_string(),
                args: vec!["--port".to_string(), "12345".to_string()],
            },
        );
        save_to(&path, &remembered);
        assert_eq!(load_from(&path), remembered);
    }

    #[test]
    fn load_on_malformed_json_returns_the_default_instead_of_erroring() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("debug_adapters.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(load_from(&path), DebugAdapterConfig::default());
    }

    #[test]
    fn save_creates_the_parent_directory_if_it_does_not_exist_yet() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/deeper/debug_adapters.json");
        assert!(!path.parent().unwrap().exists());
        let mut config = DebugAdapterConfig::default();
        config.adapters.insert(
            "Go".to_string(),
            DebugAdapterEntry {
                command: "dlv".to_string(),
                args: vec![],
            },
        );
        save_to(&path, &config);
        assert!(path.exists());
    }

    #[test]
    fn save_on_an_unresolvable_parent_is_a_silent_no_op() {
        save_to(Path::new("/"), &DebugAdapterConfig::default());
    }

    #[test]
    fn args_default_to_empty_when_omitted_from_the_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("debug_adapters.json");
        std::fs::write(&path, r#"{"adapters":{"Rust":{"command":"codelldb"}}}"#).unwrap();
        let config = load_from(&path);
        assert_eq!(
            config.adapters.get("Rust"),
            Some(&DebugAdapterEntry {
                command: "codelldb".to_string(),
                args: vec![],
            })
        );
    }

    #[test]
    fn config_file_path_from_home_joins_the_expected_relative_path() {
        let path = config_file_path_from_home(Path::new("/home/someone"));
        assert_eq!(
            path,
            PathBuf::from("/home/someone/.config/ide-tui/debug_adapters.json")
        );
    }

    #[test]
    fn config_file_path_resolves_to_some_path_in_this_test_environment() {
        assert!(config_file_path().is_some());
    }

    #[test]
    fn load_against_the_real_environment_never_panics() {
        let _ = load();
    }
}
