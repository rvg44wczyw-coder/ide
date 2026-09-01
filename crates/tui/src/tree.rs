//! Pure directory-tree navigation state (`docs/features/tui-shell-and-editor.md`
//! §2.3). No terminal or filesystem I/O -- everything here operates on a
//! `DirEntry` tree the caller already scanned.

use std::collections::HashSet;
use std::path::PathBuf;

use ide_core::{DirEntry, DirEntryKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRow {
    pub path: PathBuf,
    pub depth: usize,
    pub is_dir: bool,
    pub expanded: bool,
}

pub struct TreeState {
    expanded: HashSet<PathBuf>,
    selected: usize,
}

impl Default for TreeState {
    fn default() -> Self {
        Self::new()
    }
}

impl TreeState {
    pub fn new() -> Self {
        Self {
            expanded: HashSet::new(),
            selected: 0,
        }
    }

    /// `root` itself is never a row -- only its children, depth-first,
    /// limited to what's currently expanded.
    pub fn visible_rows(&self, root: &DirEntry) -> Vec<TreeRow> {
        let mut rows = Vec::new();
        for child in &root.children {
            self.push_rows(child, 0, &mut rows);
        }
        rows
    }

    fn push_rows(&self, entry: &DirEntry, depth: usize, rows: &mut Vec<TreeRow>) {
        let is_dir = entry.kind == DirEntryKind::Dir;
        let expanded = is_dir && self.expanded.contains(&entry.path);
        rows.push(TreeRow {
            path: entry.path.clone(),
            depth,
            is_dir,
            expanded,
        });
        if expanded {
            for child in &entry.children {
                self.push_rows(child, depth + 1, rows);
            }
        }
    }

    pub fn move_selection(&mut self, root: &DirEntry, delta: isize) {
        let len = self.visible_rows(root).len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let current = self.selected.min(len - 1) as isize;
        let next = (current + delta).clamp(0, len as isize - 1);
        self.selected = next as usize;
    }

    pub fn toggle_expand_selected(&mut self, root: &DirEntry) {
        let rows = self.visible_rows(root);
        let Some(row) = rows.get(self.selected) else {
            return;
        };
        if !row.is_dir {
            return;
        }
        if !self.expanded.remove(&row.path) {
            self.expanded.insert(row.path.clone());
        }
    }

    pub fn selected_row<'a>(&self, rows: &'a [TreeRow]) -> Option<&'a TreeRow> {
        rows.get(self.selected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str, path: &str, children: Vec<DirEntry>) -> DirEntry {
        DirEntry {
            name: name.to_string(),
            path: PathBuf::from(path),
            kind: DirEntryKind::Dir,
            children,
        }
    }

    fn file(name: &str, path: &str) -> DirEntry {
        DirEntry {
            name: name.to_string(),
            path: PathBuf::from(path),
            kind: DirEntryKind::File,
            children: vec![],
        }
    }

    fn sample_tree() -> DirEntry {
        dir(
            "root",
            "/root",
            vec![
                dir(
                    "src",
                    "/root/src",
                    vec![file("main.rs", "/root/src/main.rs")],
                ),
                file("Cargo.toml", "/root/Cargo.toml"),
            ],
        )
    }

    #[test]
    fn visible_rows_on_empty_tree_is_empty() {
        let root = dir("root", "/root", vec![]);
        let state = TreeState::new();
        assert!(state.visible_rows(&root).is_empty());
    }

    #[test]
    fn root_itself_is_never_a_row() {
        let root = sample_tree();
        let state = TreeState::new();
        let rows = state.visible_rows(&root);
        assert!(rows.iter().all(|r| r.path != root.path));
    }

    #[test]
    fn nothing_expanded_by_default_hides_nested_children() {
        let root = sample_tree();
        let state = TreeState::new();
        let rows = state.visible_rows(&root);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.depth == 0));
    }

    #[test]
    fn expanding_a_directory_reveals_its_children_at_the_next_depth() {
        let root = sample_tree();
        let mut state = TreeState::new();
        state.move_selection(&root, 0); // select the first row (src)
        state.toggle_expand_selected(&root);
        let rows = state.visible_rows(&root);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].path, PathBuf::from("/root/src"));
        assert!(rows[0].expanded);
        assert_eq!(rows[1].path, PathBuf::from("/root/src/main.rs"));
        assert_eq!(rows[1].depth, 1);
        assert_eq!(rows[2].path, PathBuf::from("/root/Cargo.toml"));
        assert_eq!(rows[2].depth, 0);
    }

    #[test]
    fn toggling_twice_collapses_again() {
        let root = sample_tree();
        let mut state = TreeState::new();
        state.toggle_expand_selected(&root);
        state.toggle_expand_selected(&root);
        let rows = state.visible_rows(&root);
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn toggle_on_a_file_row_is_a_no_op() {
        let root = sample_tree();
        let mut state = TreeState::new();
        state.move_selection(&root, 1); // select Cargo.toml (a file)
        state.toggle_expand_selected(&root);
        let rows = state.visible_rows(&root);
        assert_eq!(rows.len(), 2, "toggling a file row must not change rows");
    }

    #[test]
    fn move_selection_clamps_at_both_ends() {
        let root = sample_tree();
        let mut state = TreeState::new();
        state.move_selection(&root, -5);
        assert_eq!(state.selected, 0);
        state.move_selection(&root, 5);
        assert_eq!(state.selected, 1);
    }

    #[test]
    fn move_selection_on_empty_tree_is_a_no_op() {
        let root = dir("root", "/root", vec![]);
        let mut state = TreeState::new();
        state.move_selection(&root, 3);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn selected_row_returns_the_row_at_the_selected_index() {
        let root = sample_tree();
        let mut state = TreeState::new();
        state.move_selection(&root, 1);
        let rows = state.visible_rows(&root);
        let selected = state.selected_row(&rows).unwrap();
        assert_eq!(selected.path, PathBuf::from("/root/Cargo.toml"));
    }

    #[test]
    fn selected_row_out_of_range_returns_none() {
        let rows: Vec<TreeRow> = vec![];
        let state = TreeState::new();
        assert!(state.selected_row(&rows).is_none());
    }
}
