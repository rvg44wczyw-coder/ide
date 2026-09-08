//! Per-user (home-directory-scoped) settings storage
//! (`docs/features/settings-window.md` §2.1): a generic JSON read/write
//! helper, deliberately parallel to [`crate::project_settings`]'s
//! per-project one -- generic over the payload type
//! (`T: Serialize`/`DeserializeOwned`) so this module never needs to know
//! about `ide-ui`-only types (`Theme`, `KeymapOverlay`). Unlike
//! `project_settings`, there is exactly one file, not a multi-slot enum --
//! nothing needs a second user-level JSON blob yet.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// `~/.config/ide` (or `%USERPROFILE%\.config\ide` on Windows) -- same
/// `HOME`-then-`USERPROFILE` resolution `crates/tui/src/keymap.rs::
/// keymap_file_path` already uses. `None` if neither variable is set.
pub fn config_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(Path::new(&home).join(".config/ide"))
}

/// `config_dir().map(|d| d.join("settings.json"))`.
pub fn settings_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("settings.json"))
}

/// Reads and deserializes the file at `path`. `None` if it doesn't exist,
/// can't be read, or doesn't parse -- a hand-edited or crash-truncated
/// file falls back to `T::default()` at the call site, never panics.
pub fn read_from<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Convenience wrapper: `settings_path()` then `read_from`. `None` if
/// `config_dir()` itself can't be resolved (no `HOME`/`USERPROFILE` --
/// treated as "use defaults," not an error).
pub fn read<T: serde::de::DeserializeOwned>() -> Option<T> {
    read_from(&settings_path()?)
}

/// Serializes `value` and writes it to `path`, pretty-printed, atomically
/// (temp file in the same directory via [`tempfile::Builder`], then
/// `persist` -- same collision-proof discipline as
/// `project_settings::write`). Creates the parent directory if it doesn't
/// exist.
pub fn write_to<T: serde::Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let Some(dir) = path.parent() else {
        return Ok(());
    };
    fs::create_dir_all(dir)?;
    let json = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".settings-")
        .tempfile_in(dir)?;
    tmp.write_all(&json)?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Convenience wrapper: `settings_path()` then `write_to`. No-ops
/// (returns `Ok(())`) if `config_dir()` can't be resolved -- best-effort,
/// matching `flush_project_settings`'s existing "a write failure is
/// swallowed, this frame's settings simply don't persist" posture.
pub fn write<T: serde::Serialize>(value: &T) -> std::io::Result<()> {
    let Some(path) = settings_path() else {
        return Ok(());
    };
    write_to(&path, value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
    struct Example {
        count: u32,
    }

    #[test]
    fn config_dir_resolves_to_some_path_in_this_test_environment() {
        assert!(config_dir().is_some());
    }

    #[test]
    fn settings_path_is_config_dir_joined_with_settings_json() {
        let dir = config_dir().unwrap();
        assert_eq!(settings_path().unwrap(), dir.join("settings.json"));
    }

    #[test]
    fn read_against_the_real_environment_never_panics() {
        // Read-only, so safe to run for real (mirrors
        // `crates/tui/src/keymap.rs`'s equivalent `load()` smoke test) --
        // `write()` is deliberately never exercised against the real
        // environment, since it would touch the developer's actual
        // `~/.config/ide/settings.json`.
        let _ = read::<Example>();
    }

    #[test]
    fn read_from_with_no_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        assert_eq!(read_from::<Example>(&path), None);
    }

    #[test]
    fn write_to_then_read_from_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        write_to(&path, &Example { count: 3 }).unwrap();

        assert_eq!(read_from::<Example>(&path), Some(Example { count: 3 }));
    }

    #[test]
    fn write_to_creates_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("dir").join("settings.json");
        write_to(&path, &Example { count: 1 }).unwrap();

        assert!(path.exists());
    }

    #[test]
    fn read_from_on_malformed_json_returns_none_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, b"{ not json").unwrap();

        assert_eq!(read_from::<Example>(&path), None);
    }

    #[test]
    fn write_to_leaves_no_temp_file_behind_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        write_to(&path, &Example { count: 1 }).unwrap();

        let entries: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("settings.json")]);
    }

    #[test]
    fn write_to_overwrites_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        write_to(&path, &Example { count: 1 }).unwrap();
        write_to(&path, &Example { count: 2 }).unwrap();

        assert_eq!(read_from::<Example>(&path), Some(Example { count: 2 }));
    }

    #[test]
    fn concurrent_writes_never_corrupt_the_final_file() {
        use std::sync::Arc;
        use std::thread;

        let dir = tempfile::tempdir().unwrap();
        let path: Arc<PathBuf> = Arc::new(dir.path().join("settings.json"));

        for round in 0..20 {
            let handles: Vec<_> = (0..16u32)
                .map(|i| {
                    let path = Arc::clone(&path);
                    thread::spawn(move || write_to(&path, &Example { count: i }))
                })
                .collect();
            for h in handles {
                h.join().unwrap().unwrap();
            }

            let got = read_from::<Example>(&path);
            assert!(got.is_some(), "round {round}: final file failed to parse");
        }
    }
}
