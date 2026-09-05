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

    /// `Right` arrow (`docs/features/tui-tree-arrow-expand-collapse.md`
    /// §2.3/§3.1): a collapsed directory expands in place; an already-
    /// expanded directory moves the selection to its first child, guarded
    /// by an explicit depth check -- `visible_rows()`'s depth-first order
    /// only guarantees a child sits at `selected + 1` when one exists, so
    /// an empty expanded directory's `selected + 1` (if any) is a sibling
    /// or an ancestor's sibling, never a child, and must not be descended
    /// into. No-op on a file row.
    pub fn expand_or_descend_selected(&mut self, root: &DirEntry) {
        let rows = self.visible_rows(root);
        let Some(row) = rows.get(self.selected) else {
            return;
        };
        if !row.is_dir {
            return;
        }
        if self.expanded.insert(row.path.clone()) {
            return;
        }
        let is_child = rows
            .get(self.selected + 1)
            .is_some_and(|next| next.depth == row.depth + 1);
        if is_child {
            self.selected += 1;
        }
    }

    /// `Left` arrow (`docs/features/tui-tree-arrow-expand-collapse.md`
    /// §2.3/§3.2): an expanded directory collapses in place; otherwise
    /// (a file row, or an already-collapsed directory) the selection moves
    /// to its parent row -- the nearest earlier row whose `depth` is
    /// exactly one less than the selected row's. No-op at `depth == 0`
    /// (no parent row exists).
    pub fn collapse_or_ascend_selected(&mut self, root: &DirEntry) {
        let rows = self.visible_rows(root);
        let Some(row) = rows.get(self.selected) else {
            return;
        };
        if row.is_dir && self.expanded.remove(&row.path) {
            return;
        }
        if row.depth == 0 {
            return;
        }
        let target_depth = row.depth - 1;
        if let Some(parent_idx) = rows[..self.selected]
            .iter()
            .rposition(|r| r.depth == target_depth)
        {
            self.selected = parent_idx;
        }
    }

    pub fn selected_row<'a>(&self, rows: &'a [TreeRow]) -> Option<&'a TreeRow> {
        rows.get(self.selected)
    }

    /// Sets the selection directly to `index`, clamped to the currently
    /// visible row count -- unlike `move_selection`'s relative delta, a
    /// mouse click already knows exactly which row it landed on
    /// (`docs/features/tui-mouse-support.md` §3.2.1). A no-op (selection
    /// left at 0) on an empty tree, same as `move_selection`.
    pub fn select(&mut self, root: &DirEntry, index: usize) {
        let len = self.visible_rows(root).len();
        self.selected = if len == 0 { 0 } else { index.min(len - 1) };
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

    #[test]
    fn select_sets_the_selection_directly() {
        let root = sample_tree();
        let mut state = TreeState::new();
        state.select(&root, 1);
        assert_eq!(state.selected, 1);
    }

    #[test]
    fn select_clamps_past_the_last_row() {
        let root = sample_tree();
        let mut state = TreeState::new();
        state.select(&root, 99);
        assert_eq!(state.selected, 1);
    }

    #[test]
    fn select_on_an_empty_tree_is_a_no_op() {
        let root = dir("root", "/root", vec![]);
        let mut state = TreeState::new();
        state.select(&root, 3);
        assert_eq!(state.selected, 0);
    }

    // -- T40: Left/Right arrow expand/collapse
    // (`tui-tree-arrow-expand-collapse.md`) --

    /// `src/` (containing `lib/` (containing `mod.rs`) and `main.rs`),
    /// `empty/` (no children), `Cargo.toml` -- deep and wide enough to
    /// exercise multi-level ascend and the empty-directory descend guard.
    fn deep_tree() -> DirEntry {
        dir(
            "root",
            "/root",
            vec![
                dir(
                    "src",
                    "/root/src",
                    vec![
                        dir(
                            "lib",
                            "/root/src/lib",
                            vec![file("mod.rs", "/root/src/lib/mod.rs")],
                        ),
                        file("main.rs", "/root/src/main.rs"),
                    ],
                ),
                dir("empty", "/root/empty", vec![]),
                file("Cargo.toml", "/root/Cargo.toml"),
            ],
        )
    }

    #[test]
    fn right_on_a_collapsed_directory_expands_it_and_keeps_selection() {
        let root = deep_tree();
        let mut state = TreeState::new();
        state.select(&root, 0); // src/

        state.expand_or_descend_selected(&root);

        let rows = state.visible_rows(&root);
        assert!(rows[0].expanded);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn right_on_an_expanded_directory_descends_to_its_first_child() {
        let root = deep_tree();
        let mut state = TreeState::new();
        state.select(&root, 0); // src/
        state.expand_or_descend_selected(&root); // expand

        state.expand_or_descend_selected(&root); // descend

        let rows = state.visible_rows(&root);
        assert_eq!(rows[state.selected].path, PathBuf::from("/root/src/lib"));
    }

    #[test]
    fn right_twice_more_descends_through_nested_directories() {
        let root = deep_tree();
        let mut state = TreeState::new();
        state.select(&root, 0); // src/
        state.expand_or_descend_selected(&root); // expand src/
        state.expand_or_descend_selected(&root); // -> lib/
        state.expand_or_descend_selected(&root); // expand lib/

        state.expand_or_descend_selected(&root); // -> mod.rs

        let rows = state.visible_rows(&root);
        assert_eq!(
            rows[state.selected].path,
            PathBuf::from("/root/src/lib/mod.rs")
        );
    }

    #[test]
    fn right_on_an_expanded_empty_directory_is_a_noop_not_a_sibling_jump() {
        let root = deep_tree();
        let mut state = TreeState::new();
        // rows when collapsed: src/ (0), empty/ (1), Cargo.toml (2)
        state.select(&root, 1); // empty/
        state.expand_or_descend_selected(&root); // expand empty/ (still no children)

        state.expand_or_descend_selected(&root); // must NOT jump to Cargo.toml

        let rows = state.visible_rows(&root);
        assert_eq!(rows[state.selected].path, PathBuf::from("/root/empty"));
    }

    #[test]
    fn right_on_a_file_is_a_noop() {
        let root = deep_tree();
        let mut state = TreeState::new();
        state.select(&root, 2); // Cargo.toml

        state.expand_or_descend_selected(&root);

        assert_eq!(state.selected, 2);
    }

    #[test]
    fn right_on_empty_tree_is_a_noop() {
        let root = dir("root", "/root", vec![]);
        let mut state = TreeState::new();
        state.expand_or_descend_selected(&root);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn left_on_an_expanded_directory_collapses_it_and_keeps_selection() {
        let root = deep_tree();
        let mut state = TreeState::new();
        state.select(&root, 0); // src/
        state.expand_or_descend_selected(&root); // expand

        state.collapse_or_ascend_selected(&root);

        let rows = state.visible_rows(&root);
        assert!(!rows[0].expanded);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn left_on_a_file_ascends_to_its_parent_directory() {
        let root = deep_tree();
        let mut state = TreeState::new();
        state.select(&root, 0); // src/
        state.expand_or_descend_selected(&root); // expand src/
        state.expand_or_descend_selected(&root); // -> lib/
        state.expand_or_descend_selected(&root); // expand lib/
        state.expand_or_descend_selected(&root); // -> mod.rs

        state.collapse_or_ascend_selected(&root); // mod.rs isn't a dir -> ascend

        let rows = state.visible_rows(&root);
        assert_eq!(rows[state.selected].path, PathBuf::from("/root/src/lib"));
    }

    #[test]
    fn left_on_a_collapsed_directory_ascends_to_its_parent() {
        let root = deep_tree();
        let mut state = TreeState::new();
        state.select(&root, 0); // src/
        state.expand_or_descend_selected(&root); // expand src/
        state.expand_or_descend_selected(&root); // -> lib/ (still collapsed)

        state.collapse_or_ascend_selected(&root); // lib/ is collapsed -> ascend

        let rows = state.visible_rows(&root);
        assert_eq!(rows[state.selected].path, PathBuf::from("/root/src"));
    }

    #[test]
    fn left_ascends_past_a_sibling_to_the_correct_ancestor() {
        // Regression proof for the exact-depth-match requirement: after
        // expanding src/ and lib/, the visible rows are
        // src(0) lib(1) mod.rs(2) main.rs(1) empty(0) Cargo.toml(0) --
        // ascending from main.rs (depth 1) must land on src (depth 0), not
        // on lib (also depth 1, and closer in row order to mod.rs but not
        // an ancestor of main.rs).
        let root = deep_tree();
        let mut state = TreeState::new();
        state.select(&root, 0); // src/
        state.expand_or_descend_selected(&root); // expand src/: rows = src, lib, main.rs, empty, Cargo.toml
        let rows = state.visible_rows(&root);
        let main_rs_idx = rows
            .iter()
            .position(|r| r.path == std::path::Path::new("/root/src/main.rs"))
            .unwrap();
        state.select(&root, main_rs_idx);

        state.collapse_or_ascend_selected(&root);

        let rows = state.visible_rows(&root);
        assert_eq!(rows[state.selected].path, PathBuf::from("/root/src"));
    }

    #[test]
    fn left_on_a_top_level_collapsed_directory_is_a_noop() {
        let root = deep_tree();
        let mut state = TreeState::new();
        state.select(&root, 1); // empty/ (depth 0, collapsed)

        state.collapse_or_ascend_selected(&root);

        assert_eq!(state.selected, 1);
    }

    #[test]
    fn left_on_a_top_level_file_is_a_noop() {
        let root = deep_tree();
        let mut state = TreeState::new();
        state.select(&root, 2); // Cargo.toml (depth 0)

        state.collapse_or_ascend_selected(&root);

        assert_eq!(state.selected, 2);
    }

    #[test]
    fn left_on_empty_tree_is_a_noop() {
        let root = dir("root", "/root", vec![]);
        let mut state = TreeState::new();
        state.collapse_or_ascend_selected(&root);
        assert_eq!(state.selected, 0);
    }
}
