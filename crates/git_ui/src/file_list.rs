use collections::{BTreeMap, HashSet};
use gpui::SharedString;

#[derive(PartialEq, Clone, Copy)]
pub enum ViewMode {
    Flat,
    Tree,
}

pub enum DisplayEntry {
    File {
        entry_index: usize,
        depth: usize,
        display_name: SharedString,
    },
    Directory {
        path: SharedString,
        name: SharedString,
        depth: usize,
        expanded: bool,
    },
}

#[derive(Default)]
pub struct TreeNode {
    pub name: SharedString,
    pub path: Option<SharedString>,
    pub children: BTreeMap<SharedString, TreeNode>,
    pub files: Vec<(usize, SharedString)>,
}

pub fn build_file_tree(paths: &[(usize, &str)]) -> TreeNode {
    let mut root = TreeNode::default();
    for &(ix, path_str) in paths {
        let components: Vec<&str> = path_str.split('/').collect();
        if components.is_empty() {
            continue;
        }

        let mut current = &mut root;
        let mut current_path = String::new();

        for (ci, component) in components.iter().enumerate() {
            if ci == components.len() - 1 {
                current
                    .files
                    .push((ix, SharedString::from(component.to_string())));
            } else {
                if !current_path.is_empty() {
                    current_path.push('/');
                }
                current_path.push_str(component);

                let component_key = SharedString::from(component.to_string());
                current = current
                    .children
                    .entry(component_key.clone())
                    .or_insert_with(|| TreeNode {
                        name: component_key,
                        path: Some(SharedString::from(current_path.clone())),
                        ..Default::default()
                    });
            }
        }
    }
    root
}

/// Collapse a chain of single-child directories (e.g. `a/b/c`) into one display
/// row, returning the terminal node and the joined display name.
pub fn compact_directory_chain(node: &TreeNode) -> (&TreeNode, SharedString) {
    let mut current = node;
    let mut parts: Vec<SharedString> = vec![current.name.clone()];
    while current.files.is_empty() && current.children.len() == 1 {
        let child = current.children.values().next().expect("checked len == 1");
        if child.path.is_none() {
            break;
        }
        parts.push(child.name.clone());
        current = child;
    }
    let name = parts
        .iter()
        .map(|s| s.as_ref())
        .collect::<Vec<_>>()
        .join("/");
    (current, SharedString::from(name))
}

pub fn flatten_file_tree(
    node: &TreeNode,
    depth: usize,
    expanded_dirs: &HashSet<SharedString>,
    out: &mut Vec<DisplayEntry>,
) {
    for child in node.children.values() {
        let (terminal, display_name) = compact_directory_chain(child);
        let path = terminal
            .path
            .clone()
            .or_else(|| child.path.clone())
            .unwrap_or_default();
        let expanded = expanded_dirs.contains(&path);

        out.push(DisplayEntry::Directory {
            path: path.clone(),
            name: display_name,
            depth,
            expanded,
        });

        if expanded {
            flatten_file_tree(terminal, depth + 1, expanded_dirs, out);
        }
    }

    for (entry_index, file_name) in &node.files {
        out.push(DisplayEntry::File {
            entry_index: *entry_index,
            depth,
            display_name: file_name.clone(),
        });
    }
}
