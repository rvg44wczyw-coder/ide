use std::fs;
use std::path::{Path, PathBuf};

/// Canonicalizes `path` and confirms it stays within `root` (which must
/// already be canonical). `None` if the path doesn't exist, can't be
/// canonicalized, or escapes `root` — the same fail-closed discipline as
/// `ide_core::GitRepo::resolve_conflict`'s escape check, applied to both
/// outgoing (UI-sourced) and incoming (server-sourced) paths.
pub fn validate_path(root: &Path, path: &Path) -> Option<PathBuf> {
    let canonical = fs::canonicalize(path).ok()?;
    if canonical.starts_with(root) {
        Some(canonical)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    #[test]
    fn accepts_path_inside_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let file = root.join("src.rs");
        File::create(&file).unwrap();

        assert_eq!(validate_path(&root, &file), Some(file));
    }

    #[test]
    fn rejects_path_outside_root() {
        let root_dir = tempfile::tempdir().unwrap();
        let other_dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(root_dir.path()).unwrap();
        let outside = fs::canonicalize(other_dir.path())
            .unwrap()
            .join("secret.rs");
        File::create(&outside).unwrap();

        assert_eq!(validate_path(&root, &outside), None);
    }

    #[test]
    fn rejects_nonexistent_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        assert_eq!(validate_path(&root, &root.join("missing.rs")), None);
    }

    #[test]
    fn rejects_symlink_escaping_root() {
        #[cfg(unix)]
        {
            let root_dir = tempfile::tempdir().unwrap();
            let outside_dir = tempfile::tempdir().unwrap();
            let root = fs::canonicalize(root_dir.path()).unwrap();
            let outside_target = fs::canonicalize(outside_dir.path())
                .unwrap()
                .join("secret.rs");
            File::create(&outside_target).unwrap();

            let link = root.join("link.rs");
            std::os::unix::fs::symlink(&outside_target, &link).unwrap();

            assert_eq!(validate_path(&root, &link), None);
        }
    }
}
