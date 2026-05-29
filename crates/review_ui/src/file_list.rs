use collections::{BTreeMap, HashSet};
use git::repository::RepoPath;
use git::status::{TreeDiff, TreeDiffStatus};
use gpui::{Context, EventEmitter, Render, SharedString, Window, actions, px};
use ui::{
    Color, Icon, IconName, IconSize, IntoElement, Label, LabelSize, div, h_flex, prelude::*,
    v_flex,
};
use zed_actions::review_panel::OpenLocalFile;

actions!(
    review_file_list,
    [ExpandSelectedEntry, CollapseSelectedEntry]
);

const TREE_INDENT: f32 = 16.0;

pub enum FileListEvent {
    OpenFileDiff(RepoPath),
    OpenLocalFile(RepoPath),
}

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

pub struct FileList {
    base_branch: Option<SharedString>,
    head_branch: Option<SharedString>,
    entries: Vec<(RepoPath, TreeDiffStatus)>,
    viewed_files: HashSet<RepoPath>,
    selected_entry: Option<usize>,
    view_mode: ViewMode,
    expanded_dirs: HashSet<SharedString>,
    display_entries: Vec<DisplayEntry>,
}

impl EventEmitter<FileListEvent> for FileList {}

fn sort_diff_entries(tree_diff: &TreeDiff) -> Vec<(RepoPath, TreeDiffStatus)> {
    let mut entries: Vec<_> = tree_diff
        .entries
        .iter()
        .map(|(p, s)| (p.clone(), s.clone()))
        .collect();
    entries.sort_by(|(path_a, _), (path_b, _)| path_a.cmp(path_b));
    entries
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

impl FileList {
    pub fn new(
        base_branch: Option<SharedString>,
        head_branch: Option<SharedString>,
        tree_diff: Option<&TreeDiff>,
        _cx: &mut Context<Self>,
    ) -> Self {
        let entries = tree_diff.map(sort_diff_entries).unwrap_or_default();
        let mut this = Self {
            base_branch,
            head_branch,
            entries,
            viewed_files: HashSet::default(),
            selected_entry: None,
            view_mode: ViewMode::Flat,
            expanded_dirs: HashSet::default(),
            display_entries: Vec::new(),
        };
        this.rebuild_display_entries();
        this
    }

    pub fn set_diff(
        &mut self,
        base: Option<SharedString>,
        head: Option<SharedString>,
        diff: Option<&TreeDiff>,
        cx: &mut Context<Self>,
    ) {
        self.base_branch = base;
        self.head_branch = head;
        self.entries = diff.map(sort_diff_entries).unwrap_or_default();
        self.viewed_files.clear();
        self.selected_entry = None;
        self.rebuild_display_entries();
        cx.notify();
    }

    pub fn mark_viewed(&mut self, path: RepoPath, cx: &mut Context<Self>) {
        self.viewed_files.insert(path);
        cx.notify();
    }

    pub fn select_path(&mut self, path: &RepoPath, cx: &mut Context<Self>) {
        self.selected_entry = self.display_entries.iter().position(|de| match de {
            DisplayEntry::File { entry_index, .. } => {
                self.entries.get(*entry_index).is_some_and(|(p, _)| p == path)
            }
            DisplayEntry::Directory { .. } => false,
        });
        cx.notify();
    }

    fn visible_count(&self) -> usize {
        self.display_entries.len()
    }

    fn selected_file_entry(&self) -> Option<&(RepoPath, TreeDiffStatus)> {
        let selected = self.selected_entry?;
        match self.display_entries.get(selected)? {
            DisplayEntry::File { entry_index, .. } => self.entries.get(*entry_index),
            DisplayEntry::Directory { .. } => None,
        }
    }

    fn select_next(&mut self, _: &menu::SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
        let count = self.visible_count();
        if count == 0 {
            return;
        }
        let next = match self.selected_entry {
            Some(current) if current + 1 < count => current + 1,
            None => 0,
            _ => return,
        };
        self.selected_entry = Some(next);
        cx.notify();
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.visible_count();
        if count == 0 {
            return;
        }
        let prev = match self.selected_entry {
            Some(current) if current > 0 => current - 1,
            None => 0,
            _ => return,
        };
        self.selected_entry = Some(prev);
        cx.notify();
    }

    fn confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(selected) = self.selected_entry else {
            return;
        };
        match self.display_entries.get(selected) {
            Some(DisplayEntry::File { entry_index, .. }) => {
                if let Some((path, _)) = self.entries.get(*entry_index) {
                    cx.emit(FileListEvent::OpenFileDiff(path.clone()));
                }
            }
            Some(DisplayEntry::Directory { path, expanded, .. }) => {
                let path = path.clone();
                let was_expanded = *expanded;
                self.toggle_directory(&path, was_expanded);
                cx.notify();
            }
            None => {}
        }
    }

    fn open_local_file(
        &mut self,
        _: &OpenLocalFile,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((path, status)) = self.selected_file_entry() else {
            return;
        };
        if matches!(status, TreeDiffStatus::Deleted { .. }) {
            return;
        }
        cx.emit(FileListEvent::OpenLocalFile(path.clone()));
    }

    pub fn is_tree_view(&self) -> bool {
        self.view_mode == ViewMode::Tree
    }

    pub fn toggle_tree_view(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.view_mode = match self.view_mode {
            ViewMode::Flat => ViewMode::Tree,
            ViewMode::Tree => ViewMode::Flat,
        };
        self.selected_entry = None;
        self.rebuild_display_entries();
        cx.notify();
    }

    fn expand_selected(
        &mut self,
        _: &ExpandSelectedEntry,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(selected) = self.selected_entry else {
            return;
        };
        match self.display_entries.get(selected) {
            Some(DisplayEntry::Directory { path, expanded, .. }) => {
                if *expanded {
                    self.select_next(&menu::SelectNext, _window, cx);
                } else {
                    let path = path.clone();
                    self.toggle_directory(&path, false);
                    cx.notify();
                }
            }
            _ => {
                self.select_next(&menu::SelectNext, _window, cx);
            }
        }
    }

    fn collapse_selected(
        &mut self,
        _: &CollapseSelectedEntry,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(selected) = self.selected_entry else {
            return;
        };
        match self.display_entries.get(selected) {
            Some(DisplayEntry::Directory { path, expanded, .. }) => {
                if *expanded {
                    let path = path.clone();
                    self.toggle_directory(&path, true);
                    cx.notify();
                } else {
                    self.select_previous(&menu::SelectPrevious, _window, cx);
                }
            }
            _ => {
                self.select_previous(&menu::SelectPrevious, _window, cx);
            }
        }
    }

    fn toggle_directory(&mut self, path: &SharedString, was_expanded: bool) {
        if was_expanded {
            self.expanded_dirs.remove(path);
        } else {
            self.expanded_dirs.insert(path.clone());
        }
        self.rebuild_display_entries();
    }

    fn rebuild_display_entries(&mut self) {
        self.display_entries.clear();
        match self.view_mode {
            ViewMode::Flat => {
                for (ix, (path, _)) in self.entries.iter().enumerate() {
                    self.display_entries.push(DisplayEntry::File {
                        entry_index: ix,
                        depth: 0,
                        display_name: SharedString::from(
                            path.as_std_path().to_string_lossy().to_string(),
                        ),
                    });
                }
            }
            ViewMode::Tree => {
                let paths: Vec<(usize, String)> = self
                    .entries
                    .iter()
                    .enumerate()
                    .map(|(ix, (path, _))| {
                        (ix, path.as_std_path().to_string_lossy().to_string())
                    })
                    .collect();
                let indexed: Vec<(usize, &str)> =
                    paths.iter().map(|(ix, s)| (*ix, s.as_str())).collect();
                let tree = build_file_tree(&indexed);
                flatten_file_tree(&tree, 0, &self.expanded_dirs, &mut self.display_entries);
            }
        }
    }
}

impl Render for FileList {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.entries.is_empty() {
            return v_flex()
                .size_full()
                .justify_center()
                .items_center()
                .child(Label::new("No changed files").color(Color::Muted))
                .into_any_element();
        }

        let header_text = format!(
            "{} <- {}",
            self.base_branch.as_ref().map(|s| s.as_ref()).unwrap_or("?"),
            self.head_branch.as_ref().map(|s| s.as_ref()).unwrap_or("?"),
        );

        let file_count = self.entries.len();
        let added = self
            .entries
            .iter()
            .filter(|(_, s)| matches!(s, TreeDiffStatus::Added))
            .count();
        let modified = self
            .entries
            .iter()
            .filter(|(_, s)| matches!(s, TreeDiffStatus::Modified { .. }))
            .count();
        let deleted = self
            .entries
            .iter()
            .filter(|(_, s)| matches!(s, TreeDiffStatus::Deleted { .. }))
            .count();
        let viewed_count = self.viewed_files.len();

        let summary = format!(
            "{} changed files (+{} -{} ~{}) — {}/{} viewed",
            file_count, added, deleted, modified, viewed_count, file_count
        );

        let info_color = cx.theme().status().info;
        let selected_bg_alpha = 0.08;

        let mut file_rows: Vec<gpui::AnyElement> = Vec::new();

        for (ix, entry) in self.display_entries.iter().enumerate() {
            let is_selected = self.selected_entry == Some(ix);
            let bg = if is_selected {
                info_color.alpha(selected_bg_alpha)
            } else {
                cx.theme().colors().ghost_element_background
            };
            let hover_bg = if is_selected {
                info_color.alpha(selected_bg_alpha + 0.04)
            } else {
                cx.theme().colors().ghost_element_hover
            };

            match entry {
                DisplayEntry::Directory {
                    path,
                    name,
                    depth,
                    expanded,
                } => {
                    let folder_icon = if *expanded {
                        IconName::FolderOpen
                    } else {
                        IconName::Folder
                    };
                    let dir_path = path.clone();

                    file_rows.push(
                        h_flex()
                            .id(SharedString::from(format!("dir_{}", ix)))
                            .px_2()
                            .py_1()
                            .gap_2()
                            .rounded_md()
                            .cursor_pointer()
                            .bg(bg)
                            .hover(move |style| style.bg(hover_bg))
                            .pl(px(*depth as f32 * TREE_INDENT + 8.0))
                            .child(
                                Icon::new(folder_icon)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                div().overflow_x_hidden().child(
                                    Label::new(name.to_string())
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .single_line(),
                                ),
                            )
                            .on_click({
                                let dir_path = dir_path.clone();
                                let was_expanded = *expanded;
                                cx.listener(move |this, _event, _window, cx| {
                                    this.selected_entry = Some(ix);
                                    this.toggle_directory(&dir_path, was_expanded);
                                    cx.notify();
                                })
                            })
                            .into_any_element(),
                    );
                }
                DisplayEntry::File {
                    entry_index,
                    depth,
                    display_name,
                } => {
                    let Some((path, status)) = self.entries.get(*entry_index) else {
                        continue;
                    };
                    let (icon, color) = match status {
                        TreeDiffStatus::Added => (IconName::Plus, Color::Created),
                        TreeDiffStatus::Modified { .. } => (IconName::Pencil, Color::Modified),
                        TreeDiffStatus::Deleted { .. } => (IconName::Dash, Color::Deleted),
                    };
                    let is_viewed = self.viewed_files.contains(path);
                    let label_color = if is_viewed {
                        Color::Muted
                    } else {
                        Color::Default
                    };
                    let path = path.clone();
                    let indent = *depth as f32 * TREE_INDENT + 8.0;
                    let display_name = display_name.clone();

                    file_rows.push(
                        h_flex()
                            .id(SharedString::from(format!("file_entry_{}", ix)))
                            .px_2()
                            .py_1()
                            .gap_2()
                            .rounded_md()
                            .bg(bg)
                            .hover(move |style| style.bg(hover_bg))
                            .pl(px(indent))
                            .when(!is_viewed, |row| {
                                row.child(
                                    div()
                                        .flex_none()
                                        .w(px(6.))
                                        .h(px(6.))
                                        .rounded_full()
                                        .bg(cx.theme().status().info),
                                )
                            })
                            .when(is_viewed, |row| {
                                row.child(div().flex_none().w(px(6.)).h(px(6.)))
                            })
                            .child(Icon::new(icon).size(IconSize::Small).color(color))
                            .child(
                                div().overflow_x_hidden().child(
                                    Label::new(display_name.to_string())
                                        .size(LabelSize::Small)
                                        .color(label_color)
                                        .single_line(),
                                ),
                            )
                            .on_click({
                                let path = path.clone();
                                cx.listener(move |this, _event, _window, cx| {
                                    this.viewed_files.insert(path.clone());
                                    this.selected_entry = Some(ix);
                                    cx.emit(FileListEvent::OpenFileDiff(path.clone()));
                                    cx.notify();
                                })
                            })
                            .into_any_element(),
                    );
                }
            }
        }

        v_flex()
            .id("review-file-list")
            .size_full()
            .overflow_scroll()
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::open_local_file))
            .on_action(cx.listener(Self::expand_selected))
            .on_action(cx.listener(Self::collapse_selected))
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .child(
                        Label::new(header_text)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                h_flex().px_2().pb_1().child(
                    Label::new(summary)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            )
            .children(file_rows)
            .into_any_element()
    }
}
