//! `Project` = a directory on disk chosen as the editing root, plus a
//! recursive tree scan of its contents. Scanning resolves every symlink it
//! encounters, excludes any whose target lies outside the project root
//! (see `classify`), tracks the canonical directories on the *current*
//! recursion path (an ancestor stack, not a whole-scan visited set) so a
//! symlink cycle (e.g. `project/loop -> project`) terminates instead of
//! recursing forever, and caches each canonical directory's fully-scanned
//! children the first time it's walked so that N symlinks aliasing the
//! same non-ancestor directory reuse one walk of it (one `fs::read_dir`
//! subtree) instead of each re-walking it independently — without this,
//! `branch` symlinks per level across `levels` of aliasing costs
//! `O(branch^levels)` `fs::read_dir` calls, a real, measured exponential
//! blowup from a tiny on-disk symlink tree (see
//! `docs/security-findings/editor-shell-project-scan-2026-08-16.md`); with
//! it, the walk cost is `O(unique real directories under root)`. Also
//! silently skips directory entries it can't read (permission errors)
//! rather than failing the whole scan.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("path is not a directory: {0}")]
    NotADirectory(PathBuf),
    #[error("path already exists: {0}")]
    AlreadyExists(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirEntryKind {
    File,
    Dir,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub path: PathBuf,
    pub kind: DirEntryKind,
    pub children: Vec<DirEntry>,
}

pub struct Project {
    root: PathBuf,
}

impl Project {
    /// Opens an existing directory as a project root. Canonicalizes the
    /// path. Errors if `root` doesn't exist or isn't a directory.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, ProjectError> {
        let canonical = fs::canonicalize(root.as_ref())?;
        if !canonical.is_dir() {
            return Err(ProjectError::NotADirectory(canonical));
        }
        Ok(Self { root: canonical })
    }

    /// Creates `root` as a new directory and opens it as a project.
    /// Errors if `root` already exists.
    pub fn create(root: impl AsRef<Path>) -> Result<Self, ProjectError> {
        let root = root.as_ref();
        if root.exists() {
            return Err(ProjectError::AlreadyExists(root.to_path_buf()));
        }
        fs::create_dir_all(root)?;
        let canonical = fs::canonicalize(root)?;
        Ok(Self { root: canonical })
    }

    /// Canonicalized, absolute project root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Recursively scans the project tree. Entries whose canonical path
    /// escapes `root` (symlink pointing outside the project) or that
    /// cannot be read (permission error) are silently skipped, not errors.
    /// Directories sort before files; both sort case-insensitively by name.
    /// Cost is bounded by the number of unique real directories under
    /// `root`, not by how many symlinks exist or how they're arranged (see
    /// module docs).
    pub fn scan_tree(&self) -> DirEntry {
        let name = self
            .root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.root.to_string_lossy().into_owned());

        let mut state = ScanState {
            ancestors: HashSet::new(),
            cache: HashMap::new(),
        };
        state.ancestors.insert(self.root.clone());
        let mut children = read_children(&self.root, &self.root, &mut state);
        sort_entries(&mut children);

        DirEntry {
            name,
            path: self.root.clone(),
            kind: DirEntryKind::Dir,
            children,
        }
    }
}

/// Per-`scan_tree()`-call state threaded through the recursive walk.
struct ScanState {
    /// Canonical directory paths on the current recursion path — detects a
    /// symlink cycle (a directory that is its own ancestor).
    ancestors: HashSet<PathBuf>,
    /// Canonical directory path -> its already-computed, sorted children.
    /// Populated the first time a directory is fully walked; a later
    /// symlink resolving to the same canonical path reuses the cached
    /// result instead of re-walking (see module docs).
    cache: HashMap<PathBuf, Vec<DirEntry>>,
}

/// Resolves `path`'s kind, following (and validating) a symlink if it is
/// one, and returns the canonical path to use for cycle tracking (the
/// symlink's target when `path` is a symlink, `path` itself otherwise —
/// non-symlink paths are already canonical by induction, since every
/// ancestor on the walk from `canonical_root` was itself either the root
/// or a previously-validated entry). Returns `None` if the path is
/// unreadable or a symlink escapes `canonical_root`.
fn classify(canonical_root: &Path, path: &Path) -> Option<(DirEntryKind, PathBuf)> {
    let symlink_meta = fs::symlink_metadata(path).ok()?;
    if symlink_meta.file_type().is_symlink() {
        let target = fs::canonicalize(path).ok()?;
        if !target.starts_with(canonical_root) {
            return None;
        }
        let kind = if target.is_dir() {
            DirEntryKind::Dir
        } else {
            DirEntryKind::File
        };
        Some((kind, target))
    } else if symlink_meta.is_dir() {
        Some((DirEntryKind::Dir, path.to_path_buf()))
    } else {
        Some((DirEntryKind::File, path.to_path_buf()))
    }
}

fn read_children(canonical_root: &Path, dir: &Path, state: &mut ScanState) -> Vec<DirEntry> {
    let Ok(read_dir) = fs::read_dir(dir) else {
        return Vec::new();
    };
    read_dir
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| build_entry(canonical_root, &entry.path(), state))
        .collect()
}

fn build_entry(canonical_root: &Path, path: &Path, state: &mut ScanState) -> Option<DirEntry> {
    let (kind, canonical_path) = classify(canonical_root, path)?;
    let name = path.file_name()?.to_string_lossy().into_owned();
    let children = match kind {
        DirEntryKind::Dir => {
            if let Some(cached) = state.cache.get(&canonical_path) {
                // Already fully walked via an earlier alias — reuse it
                // instead of re-walking (this is what keeps N aliases of
                // the same directory linear instead of exponential).
                cached.clone()
            } else if state.ancestors.insert(canonical_path.clone()) {
                // Not on the current recursion path: push it for the
                // duration of the recursive call, pop it after, so a
                // sibling symlink aliasing the same directory can still be
                // walked (its result will hit the cache branch above).
                let mut kids = read_children(canonical_root, path, state);
                state.ancestors.remove(&canonical_path);
                sort_entries(&mut kids);
                state.cache.insert(canonical_path, kids.clone());
                kids
            } else {
                // `canonical_path` is already an ancestor: a true symlink
                // cycle. Stop here instead of recursing forever.
                Vec::new()
            }
        }
        DirEntryKind::File => Vec::new(),
    };
    Some(DirEntry {
        name,
        path: path.to_path_buf(),
        kind,
        children,
    })
}

fn sort_entries(entries: &mut [DirEntry]) {
    entries.sort_by(|a, b| match (&a.kind, &b.kind) {
        (DirEntryKind::Dir, DirEntryKind::File) => Ordering::Less,
        (DirEntryKind::File, DirEntryKind::Dir) => Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    #[test]
    fn open_existing_directory_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let project = Project::open(dir.path()).unwrap();
        assert_eq!(project.root(), fs::canonicalize(dir.path()).unwrap());
    }

    #[test]
    fn open_missing_path_errors() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert!(matches!(Project::open(&missing), Err(ProjectError::Io(_))));
    }

    #[test]
    fn open_file_not_directory_errors() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        fs::write(&file, "hi").unwrap();
        assert!(matches!(
            Project::open(&file),
            Err(ProjectError::NotADirectory(_))
        ));
    }

    #[test]
    fn create_new_directory_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("new-project");
        let project = Project::create(&target).unwrap();
        assert_eq!(project.root(), fs::canonicalize(&target).unwrap());
    }

    #[test]
    fn create_existing_path_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            Project::create(dir.path()),
            Err(ProjectError::AlreadyExists(_))
        ));
    }

    #[test]
    fn scan_tree_lists_files_and_dirs_sorted() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("b.txt"), "").unwrap();
        fs::write(dir.path().join("a.txt"), "").unwrap();
        fs::create_dir(dir.path().join("z_dir")).unwrap();

        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        assert_eq!(tree.children.len(), 3);
        // dirs sort before files; z_dir before a.txt/b.txt despite name
        assert_eq!(tree.children[0].name, "z_dir");
        assert_eq!(tree.children[0].kind, DirEntryKind::Dir);
        assert_eq!(tree.children[1].name, "a.txt");
        assert_eq!(tree.children[2].name, "b.txt");
    }

    #[test]
    fn scan_tree_recurses_into_subdirectories() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/inner.txt"), "").unwrap();

        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        assert_eq!(tree.children.len(), 1);
        assert_eq!(tree.children[0].children.len(), 1);
        assert_eq!(tree.children[0].children[0].name, "inner.txt");
    }

    #[cfg(unix)]
    #[test]
    fn scan_tree_terminates_on_symlink_cycle() {
        let dir = tempfile::tempdir().unwrap();
        // project/loop -> project itself: without cycle detection this
        // recurses forever (verified separately) and overflows the stack.
        symlink(dir.path(), dir.path().join("loop")).unwrap();

        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree(); // must return, not hang/crash

        assert_eq!(tree.children.len(), 1);
        assert_eq!(tree.children[0].name, "loop");
        // root was already an ancestor when `loop` resolved back to it, so
        // the cycle is cut here: no duplicated/infinite children.
        assert!(tree.children[0].children.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn scan_tree_memoizes_aliased_directories_instead_of_exponential_blowup() {
        // Regression test for the exact payload shape from
        // docs/security-findings/editor-shell-project-scan-2026-08-16.md,
        // finding 1: `levels` real directories, each (but the last)
        // containing `branch` symlinks that all alias the SAME next real
        // directory. Before the memoization fix this cost O(branch^levels)
        // `fs::read_dir` calls (measured: 8 levels / branch 4 took 2.5s for
        // 50,970 output nodes); with it, the walk itself is O(levels)
        // `fs::read_dir` calls (one real directory read per level, reused
        // via the cache), so the same output size should complete in a
        // small fraction of that time.
        let dir = tempfile::tempdir().unwrap();
        let levels = 8;
        let branch = 4;

        let mut level_dirs = Vec::new();
        for i in 0..levels {
            let d = dir.path().join(format!("level_{i}"));
            fs::create_dir(&d).unwrap();
            level_dirs.push(d);
        }
        for i in 0..levels - 1 {
            for b in 0..branch {
                symlink(&level_dirs[i + 1], level_dirs[i].join(format!("link_{b}"))).unwrap();
            }
        }
        fs::write(level_dirs[levels - 1].join("leaf.txt"), "x").unwrap();

        let project = Project::open(dir.path()).unwrap();
        let start = std::time::Instant::now();
        let tree = project.scan_tree();
        let elapsed = start.elapsed();

        fn count_nodes(e: &DirEntry) -> usize {
            1 + e.children.iter().map(count_nodes).sum::<usize>()
        }
        // Same output size as the unmemoized version (the doc's alias
        // semantics — every occurrence still shows full contents — didn't
        // change, only the cost of producing it did): 50,970 total nodes
        // including the root entry itself, as independently measured
        // against this exact shape in the findings doc.
        assert_eq!(count_nodes(&tree), 50_970);
        // 2.5s was the pre-fix measurement for this exact shape; require
        // well under that as evidence the underlying fs::read_dir cost is
        // no longer exponential (a regression back to per-alias re-walking
        // would blow well past this bound).
        assert!(
            elapsed.as_secs_f64() < 1.0,
            "scan_tree took {elapsed:?} for an aliased directory tree — \
             expected memoization to keep this well under 1s (pre-fix: 2.5s)"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scan_tree_excludes_symlink_escaping_root() {
        let project_dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        fs::write(outside_dir.path().join("secret.txt"), "shh").unwrap();

        symlink(outside_dir.path(), project_dir.path().join("escape")).unwrap();

        let project = Project::open(project_dir.path()).unwrap();
        let tree = project.scan_tree();

        assert!(
            tree.children.is_empty(),
            "escaping symlink must be excluded"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scan_tree_includes_symlink_within_root() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("real")).unwrap();
        fs::write(dir.path().join("real/inner.txt"), "").unwrap();
        symlink(dir.path().join("real"), dir.path().join("link")).unwrap();

        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        let names: Vec<&str> = tree.children.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"real"));
        assert!(names.contains(&"link"));
        let link_entry = tree.children.iter().find(|e| e.name == "link").unwrap();
        assert_eq!(link_entry.kind, DirEntryKind::Dir);
        assert_eq!(link_entry.children.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn scan_tree_skips_unreadable_subdirectory() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("secret.txt"), "").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let project = Project::open(dir.path()).unwrap();
        let tree = project.scan_tree();

        // restore permissions so tempdir cleanup can remove it
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

        let locked_entry = tree.children.iter().find(|e| e.name == "locked").unwrap();
        assert!(locked_entry.children.is_empty());
    }
}
