//! Cached project tree. The runtime supplies directory listings asynchronously.

use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
};

use crate::project::ProjectEntry;

#[derive(Debug, Clone)]
pub struct Explorer {
    pub open: bool,
    pub root: PathBuf,
    pub selected: usize,
    pub expanded: BTreeSet<PathBuf>,
    pub show_hidden: bool,
    pub show_ignored: bool,
    pub width: u16,
    rows: Vec<ProjectEntry>,
    children: HashMap<PathBuf, Vec<ProjectEntry>>,
    pending_selection: Option<PathBuf>,
}

impl Explorer {
    pub fn new(root: PathBuf, width: u16, show_hidden: bool, show_ignored: bool) -> Self {
        Self {
            open: false,
            root,
            selected: 0,
            expanded: BTreeSet::new(),
            show_hidden,
            show_ignored,
            width,
            rows: Vec::new(),
            children: HashMap::new(),
            pending_selection: None,
        }
    }

    pub fn rows(&self) -> &[ProjectEntry] {
        &self.rows
    }

    pub fn selected_entry(&self) -> Option<&ProjectEntry> {
        self.rows.get(self.selected)
    }

    pub fn reset(&mut self) {
        if self.pending_selection.is_none() {
            self.pending_selection = self.selected_entry().map(|entry| entry.path.clone());
        }
        self.children.clear();
        self.rows.clear();
        self.selected = 0;
    }

    /// Only expanded, visible directories are eligible for loading.
    pub fn next_directory_to_load(&self) -> Option<PathBuf> {
        if !self.open {
            return None;
        }
        if !self.children.contains_key(&self.root) {
            return Some(self.root.clone());
        }
        self.rows
            .iter()
            .find(|entry| {
                entry.is_directory()
                    && self.expanded.contains(&entry.path)
                    && !self.children.contains_key(&entry.path)
            })
            .map(|entry| entry.path.clone())
    }

    /// An empty batch also records an empty (or failed) directory as loaded.
    pub fn append_directory(&mut self, directory: &Path, entries: Vec<ProjectEntry>) {
        let children = self.children.entry(directory.to_owned()).or_default();
        for mut entry in entries {
            let Ok(relative) = entry.path.strip_prefix(&self.root) else {
                continue;
            };
            if entry.path.parent() != Some(directory) {
                continue;
            }
            entry.relative_path = relative.to_owned();
            entry.depth = relative.components().count();
            children.push(entry);
        }
        children.sort_unstable_by(|left, right| {
            right
                .is_directory()
                .cmp(&left.is_directory())
                .then_with(|| left.path.cmp(&right.path))
        });
        children.dedup_by(|left, right| left.path == right.path);
        self.rebuild_rows();
    }

    /// Preserve selection by path as streamed entries reorder the visible tree.
    pub fn rebuild_rows(&mut self) {
        let selected = self.selected_entry().map(|entry| entry.path.clone());
        self.rows.clear();
        let mut stack = self
            .children
            .get(&self.root)
            .into_iter()
            .flat_map(|entries| entries.iter().rev())
            .collect::<Vec<_>>();
        while let Some(entry) = stack.pop() {
            self.rows.push(entry.clone());
            if entry.is_directory()
                && self.expanded.contains(&entry.path)
                && let Some(children) = self.children.get(&entry.path)
            {
                stack.extend(children.iter().rev());
            }
        }
        let pending_index = self
            .pending_selection
            .as_ref()
            .and_then(|path| self.rows.iter().position(|entry| &entry.path == path));
        if let Some(index) = pending_index {
            self.selected = index;
            self.pending_selection = None;
        } else if let Some(index) = selected
            .as_ref()
            .and_then(|path| self.rows.iter().position(|entry| &entry.path == path))
        {
            self.selected = index;
        } else {
            self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        self.pending_selection = None;
        if !self.rows.is_empty() {
            self.selected =
                (self.selected as isize + delta).rem_euclid(self.rows.len() as isize) as usize;
        }
    }

    /// Right expands a directory, then moves into its first child on repetition.
    /// Enter toggles a directory. Returns false when the selection is a file.
    pub fn expand_selected(&mut self, toggle: bool) -> bool {
        self.pending_selection = None;
        let Some(entry) = self.selected_entry().filter(|entry| entry.is_directory()) else {
            return false;
        };
        let path = entry.path.clone();
        if self.expanded.insert(path.clone()) {
            self.rebuild_rows();
        } else if toggle {
            self.expanded.remove(&path);
            self.rebuild_rows();
        } else if self
            .rows
            .get(self.selected + 1)
            .is_some_and(|entry| entry.path.parent() == Some(path.as_path()))
        {
            self.selected += 1;
        }
        true
    }

    /// Left collapses an expanded directory or selects the parent of a row.
    pub fn collapse_selected(&mut self) {
        self.pending_selection = None;
        let Some(entry) = self.selected_entry() else {
            return;
        };
        let path = entry.path.clone();
        if entry.is_directory() && self.expanded.remove(&path) {
            self.rebuild_rows();
        } else if let Some(parent) = path.parent()
            && let Some(index) = self.rows.iter().position(|entry| entry.path == parent)
        {
            self.selected = index;
        }
    }

    /// Open ancestors now and select the file once its listing arrives.
    pub fn reveal(&mut self, path: &Path) {
        if !path.starts_with(&self.root) {
            return;
        }
        let mut parent = path.parent();
        while let Some(directory) = parent {
            if directory == self.root {
                break;
            }
            self.expanded.insert(directory.to_owned());
            parent = directory.parent();
        }
        self.pending_selection = Some(path.to_owned());
        self.rebuild_rows();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::ProjectEntryKind;

    fn tree() -> Explorer {
        let mut tree = Explorer::new(PathBuf::from("/project"), 30, false, false);
        tree.open = true;
        tree
    }

    fn append(tree: &mut Explorer, directory: &str, names: &[&str]) {
        let directory = tree.root.join(directory);
        tree.append_directory(
            &directory,
            names
                .iter()
                .map(|name| {
                    let is_directory = name.ends_with('/');
                    let name = name.trim_end_matches('/');
                    ProjectEntry {
                        path: directory.join(name),
                        relative_path: name.into(),
                        kind: if is_directory {
                            ProjectEntryKind::Directory
                        } else {
                            ProjectEntryKind::File
                        },
                        depth: 1,
                    }
                })
                .collect(),
        );
    }

    fn paths(tree: &Explorer) -> Vec<String> {
        tree.rows()
            .iter()
            .map(|entry| entry.relative_path.display().to_string())
            .collect()
    }

    #[test]
    fn nested_navigation_keeps_collapsed_children_out_of_selection() {
        let mut tree = tree();
        append(&mut tree, "", &["z.rs", "src/", "a.rs", "empty/"]);
        assert_eq!(paths(&tree), ["empty", "src", "a.rs", "z.rs"]);
        assert_eq!(tree.next_directory_to_load(), None);
        tree.move_selection(1);
        assert!(tree.expand_selected(false));
        assert_eq!(tree.next_directory_to_load(), Some(tree.root.join("src")));
        append(&mut tree, "src", &["main.rs", "nested/"]);
        tree.expand_selected(false);
        assert_eq!(
            tree.selected_entry().unwrap().relative_path,
            Path::new("src/nested")
        );
        tree.expand_selected(true);
        append(&mut tree, "src/nested", &["lib.rs"]);
        tree.expand_selected(false);
        assert_eq!(tree.selected_entry().unwrap().depth, 3);
        tree.collapse_selected(); // file -> parent
        assert_eq!(
            tree.selected_entry().unwrap().relative_path,
            Path::new("src/nested")
        );
        tree.collapse_selected(); // collapse nested
        assert!(!paths(&tree).contains(&"src/nested/lib.rs".into()));
        tree.collapse_selected(); // nested -> src
        tree.expand_selected(true); // collapse src
        assert_eq!(paths(&tree), ["empty", "src", "a.rs", "z.rs"]);
        tree.move_selection(1);
        assert_eq!(
            tree.selected_entry().unwrap().relative_path,
            Path::new("a.rs")
        );
        assert!(!tree.expand_selected(true));
    }

    #[test]
    fn empty_directories_load_once_and_reopening_uses_cached_children() {
        let mut tree = tree();
        append(&mut tree, "", &["empty/"]);
        tree.expand_selected(true);
        append(&mut tree, "empty", &[]);
        assert_eq!(tree.next_directory_to_load(), None);
        tree.expand_selected(false);
        assert_eq!(tree.selected, 0);
        tree.expand_selected(true);
        tree.expand_selected(true);
        assert_eq!(tree.next_directory_to_load(), None);
        assert_eq!(tree.rows().len(), 1);
    }

    #[test]
    fn streamed_entries_and_refresh_keep_selection_and_expansion() {
        let mut tree = tree();
        append(&mut tree, "", &["src/", "z.rs"]);
        tree.expand_selected(true);
        append(&mut tree, "src", &["z.rs"]);
        tree.move_selection(1);
        append(&mut tree, "src", &["a.rs"]);
        append(&mut tree, "", &["empty/"]);
        assert_eq!(
            tree.selected_entry().unwrap().relative_path,
            Path::new("src/z.rs")
        );
        tree.reset();
        append(&mut tree, "", &["src/", "z.rs", "empty/"]);
        append(&mut tree, "src", &["a.rs", "z.rs"]);
        assert_eq!(
            tree.selected_entry().unwrap().relative_path,
            Path::new("src/z.rs")
        );
        assert!(tree.expanded.contains(&tree.root.join("src")));
    }

    #[test]
    fn reveal_waits_for_ancestors_but_user_navigation_cancels_pending_selection() {
        let mut tree = tree();
        tree.reveal(Path::new("/project/src/nested/lib.rs"));
        append(&mut tree, "", &["src/"]);
        append(&mut tree, "src", &["nested/"]);
        append(&mut tree, "src/nested", &["lib.rs"]);
        assert_eq!(
            tree.selected_entry().unwrap().relative_path,
            Path::new("src/nested/lib.rs")
        );
        tree.reveal(Path::new("/project/src/nested/z.rs"));
        tree.move_selection(-1);
        append(&mut tree, "src/nested", &["z.rs"]);
        assert_eq!(
            tree.selected_entry().unwrap().relative_path,
            Path::new("src/nested")
        );
    }
}
