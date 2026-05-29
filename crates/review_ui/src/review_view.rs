use crate::comment_card::CommentCard;
use crate::file_list::{DisplayEntry, ViewMode, build_file_tree, flatten_file_tree};
use crate::review_provider::{
    FileChangeStatus, PullRequestFile, PullRequestInfo, ReviewComment, ReviewProvider, ReviewStatus,
};
use collections::{HashMap, HashSet};
use editor::Editor;
use git::repository::RepoPath;
use git::status::{TreeDiff, TreeDiffStatus};
use gpui::{Anchor, Context, Entity, EventEmitter, Focusable, Render, SharedString, Window, px};
use std::sync::Arc;
use ui::{
    ButtonLike, ButtonSize, Color, ContextMenu, ElevationIndex, Icon, IconButton, IconName,
    IconSize, IntoElement, Label, LabelSize, PopoverMenu, PopoverMenuHandle, SplitButton, Tooltip,
    div, h_flex, prelude::*, v_flex,
};

pub enum ReviewViewEvent {
    OpenFileDiff(RepoPath),
    Back,
}

const TREE_INDENT: f32 = 16.0;

pub struct ReviewView {
    provider: Option<Arc<dyn ReviewProvider>>,
    remote_owner: Option<String>,
    remote_repo: Option<String>,
    selected_pr: PullRequestInfo,
    pr_comments: Vec<ReviewComment>,
    pr_comments_loading: bool,
    pr_api_files: Vec<PullRequestFile>,
    tree_diff: Option<TreeDiff>,
    comment_editor: Entity<Editor>,
    comment_submitting: bool,
    review_action: ReviewStatus,
    review_action_menu_handle: PopoverMenuHandle<ContextMenu>,
    view_mode: ViewMode,
    expanded_dirs: HashSet<SharedString>,
    display_entries: Vec<DisplayEntry>,
    expanded_comment_files: HashSet<SharedString>,
}

impl EventEmitter<ReviewViewEvent> for ReviewView {}

impl ReviewView {
    pub fn new(
        provider: Option<Arc<dyn ReviewProvider>>,
        remote_owner: Option<String>,
        remote_repo: Option<String>,
        pull_request: PullRequestInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let comment_editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(3, 6, window, cx);
            editor.set_placeholder_text("Leave a comment…", window, cx);
            editor.set_show_gutter(false, cx);
            editor.set_show_wrap_guides(false, cx);
            editor.set_show_indent_guides(false, cx);
            editor.set_use_autoclose(false);
            editor
        });

        let pr_number = pull_request.number;
        let mut this = Self {
            provider,
            remote_owner,
            remote_repo,
            selected_pr: pull_request,
            pr_comments: Vec::new(),
            pr_comments_loading: false,
            pr_api_files: Vec::new(),
            tree_diff: None,
            comment_editor,
            comment_submitting: false,
            review_action: ReviewStatus::Commented,
            review_action_menu_handle: PopoverMenuHandle::default(),
            view_mode: ViewMode::Flat,
            expanded_dirs: HashSet::default(),
            display_entries: Vec::new(),
            expanded_comment_files: HashSet::default(),
        };
        this.load_pr_comments(pr_number, cx);
        this.load_pr_api_files(pr_number, cx);
        this
    }

    pub fn set_tree_diff(&mut self, tree_diff: Option<&TreeDiff>, cx: &mut Context<Self>) {
        self.tree_diff = tree_diff.map(|td| TreeDiff {
            entries: td.entries.clone(),
        });
        self.rebuild_display_entries();
        cx.notify();
    }

    pub fn is_tree_view(&self) -> bool {
        self.view_mode == ViewMode::Tree
    }

    pub fn toggle_tree_view(&mut self, cx: &mut Context<Self>) {
        self.view_mode = match self.view_mode {
            ViewMode::Flat => ViewMode::Tree,
            ViewMode::Tree => ViewMode::Flat,
        };
        self.rebuild_display_entries();
        cx.notify();
    }

    fn rebuild_display_entries(&mut self) {
        self.display_entries.clear();

        let file_entries = self.collect_file_entries();

        match self.view_mode {
            ViewMode::Flat => {
                for (ix, (path, _, _, _)) in file_entries.iter().enumerate() {
                    self.display_entries.push(DisplayEntry::File {
                        entry_index: ix,
                        depth: 0,
                        display_name: path.clone(),
                    });
                }
            }
            ViewMode::Tree => {
                let paths: Vec<(usize, &str)> = file_entries
                    .iter()
                    .enumerate()
                    .map(|(ix, (path, _, _, _))| (ix, path.as_ref()))
                    .collect();
                let tree = build_file_tree(&paths);
                flatten_file_tree(&tree, 0, &self.expanded_dirs, &mut self.display_entries);
            }
        }
    }

    fn collect_file_entries(&self) -> Vec<(SharedString, Option<FileChangeStatus>, u32, u32)> {
        if !self.pr_api_files.is_empty() {
            self.pr_api_files
                .iter()
                .map(|f| {
                    (
                        f.path.clone(),
                        Some(f.status.clone()),
                        f.additions,
                        f.deletions,
                    )
                })
                .collect()
        } else if let Some(tree_diff) = &self.tree_diff {
            let mut entries: Vec<_> = tree_diff.entries.iter().collect();
            entries.sort_by_key(|(a, _)| *a);
            entries
                .into_iter()
                .map(|(path, status)| {
                    let file_status = match status {
                        TreeDiffStatus::Added => Some(FileChangeStatus::Added),
                        TreeDiffStatus::Modified { .. } => Some(FileChangeStatus::Modified),
                        TreeDiffStatus::Deleted { .. } => Some(FileChangeStatus::Deleted),
                    };
                    (
                        SharedString::from(path.as_std_path().to_string_lossy().to_string()),
                        file_status,
                        0,
                        0,
                    )
                })
                .collect()
        } else {
            Vec::new()
        }
    }

    pub fn pr_comments(&self) -> &[ReviewComment] {
        &self.pr_comments
    }

    pub fn comments_for_file(&self, path: &SharedString) -> Vec<ReviewComment> {
        let mut parent_comments: Vec<ReviewComment> = Vec::new();
        let mut replies: Vec<ReviewComment> = Vec::new();

        for comment in &self.pr_comments {
            if comment.path.as_ref() == Some(path) {
                if comment.reply_to.is_some() {
                    replies.push(comment.clone());
                } else {
                    parent_comments.push(comment.clone());
                }
            }
        }

        let mut result = Vec::new();
        for parent in parent_comments {
            let parent_id = parent.id;
            result.push(parent);
            for reply in &replies {
                if reply.reply_to == Some(parent_id) {
                    result.push(reply.clone());
                }
            }
        }

        result
    }

    fn load_pr_comments(&mut self, pr_number: u32, cx: &mut Context<Self>) {
        let Some(provider) = self.provider.clone() else {
            return;
        };
        let Some(owner) = self.remote_owner.clone() else {
            return;
        };
        let Some(repo) = self.remote_repo.clone() else {
            return;
        };

        self.pr_comments_loading = true;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let comments = provider.fetch_reviews(&owner, &repo, pr_number).await?;
            this.update(cx, |this, cx| {
                this.pr_comments = comments;
                this.pr_comments_loading = false;
                cx.notify();
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn load_pr_api_files(&mut self, pr_number: u32, cx: &mut Context<Self>) {
        let Some(provider) = self.provider.clone() else {
            return;
        };
        let Some(owner) = self.remote_owner.clone() else {
            return;
        };
        let Some(repo) = self.remote_repo.clone() else {
            return;
        };

        cx.spawn(async move |this, cx| {
            let files = provider
                .fetch_pull_request_files(&owner, &repo, pr_number)
                .await?;
            this.update(cx, |this, cx| {
                this.pr_api_files = files;
                this.rebuild_display_entries();
                cx.notify();
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn submit_review_action(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(provider) = self.provider.clone() else {
            return;
        };
        let Some(owner) = self.remote_owner.clone() else {
            return;
        };
        let Some(repo) = self.remote_repo.clone() else {
            return;
        };

        let body = self.comment_editor.read(cx).text(cx);
        let action = self.review_action.clone();
        let pr_number = self.selected_pr.number;
        self.comment_submitting = true;
        cx.notify();

        match action {
            ReviewStatus::Commented => {
                if body.trim().is_empty() {
                    self.comment_submitting = false;
                    return;
                }
                cx.spawn_in(window, async move |this, cx| {
                    let new_comment = provider
                        .submit_comment(&owner, &repo, pr_number, &body, None, None)
                        .await?;
                    this.update_in(cx, |this, window, cx| {
                        this.pr_comments.push(new_comment);
                        this.comment_submitting = false;
                        this.comment_editor.update(cx, |editor, cx| {
                            editor.clear(window, cx);
                        });
                        cx.notify();
                    })?;
                    anyhow::Ok(())
                })
                .detach_and_log_err(cx);
            }
            ReviewStatus::Approved | ReviewStatus::ChangesRequested => {
                let body_opt = if body.trim().is_empty() {
                    None
                } else {
                    Some(body)
                };
                cx.spawn_in(window, async move |this, cx| {
                    provider
                        .submit_review(&owner, &repo, pr_number, action, body_opt.as_deref())
                        .await?;
                    this.update_in(cx, |this, _window, cx| {
                        this.comment_submitting = false;
                        this.load_pr_comments(pr_number, cx);
                        cx.notify();
                    })?;
                    anyhow::Ok(())
                })
                .detach_and_log_err(cx);
            }
            ReviewStatus::Pending => {}
        }
    }

    fn review_action_label(&self) -> &'static str {
        match &self.review_action {
            ReviewStatus::Commented => "Comment",
            ReviewStatus::Approved => "Approve",
            ReviewStatus::ChangesRequested => "Request Changes",
            ReviewStatus::Pending => "Comment",
        }
    }

    fn render_review_action_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let label = self.review_action_label();
        let is_submitting = self.comment_submitting;

        let label_color = if is_submitting {
            Color::Disabled
        } else {
            Color::Default
        };

        SplitButton::new(
            ButtonLike::new_rounded_left("review-submit-left")
                .layer(ElevationIndex::ModalSurface)
                .size(ButtonSize::Compact)
                .disabled(is_submitting)
                .child(Label::new(label).size(LabelSize::Small).color(label_color))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.submit_review_action(window, cx);
                })),
            self.render_review_action_menu(cx).into_any_element(),
        )
    }

    fn render_review_action_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let weak_view = cx.weak_entity();
        let current = self.review_action.clone();

        PopoverMenu::new("review-action-menu")
            .trigger(
                ButtonLike::new_rounded_right("review-action-menu-trigger")
                    .layer(ElevationIndex::ModalSurface)
                    .size(ButtonSize::None)
                    .child(
                        h_flex()
                            .px_1()
                            .h_full()
                            .justify_center()
                            .border_l_1()
                            .border_color(cx.theme().colors().border)
                            .child(Icon::new(IconName::ChevronDown).size(IconSize::XSmall)),
                    ),
            )
            .with_handle(self.review_action_menu_handle.clone())
            .anchor(Anchor::TopRight)
            .menu(move |window, cx| {
                let weak_view = weak_view.clone();
                let current = current.clone();
                Some(ContextMenu::build(window, cx, move |menu, _window, _cx| {
                    menu.toggleable_entry(
                        "Comment",
                        matches!(current, ReviewStatus::Commented),
                        IconPosition::Start,
                        None,
                        {
                            let weak_view = weak_view.clone();
                            move |_window, cx| {
                                weak_view
                                    .update(cx, |this, cx| {
                                        this.review_action = ReviewStatus::Commented;
                                        cx.notify();
                                    })
                                    .ok();
                            }
                        },
                    )
                    .toggleable_entry(
                        "Approve",
                        matches!(current, ReviewStatus::Approved),
                        IconPosition::Start,
                        None,
                        {
                            let weak_view = weak_view.clone();
                            move |_window, cx| {
                                weak_view
                                    .update(cx, |this, cx| {
                                        this.review_action = ReviewStatus::Approved;
                                        cx.notify();
                                    })
                                    .ok();
                            }
                        },
                    )
                    .toggleable_entry(
                        "Request Changes",
                        matches!(current, ReviewStatus::ChangesRequested),
                        IconPosition::Start,
                        None,
                        {
                            move |_window, cx| {
                                weak_view
                                    .update(cx, |this, cx| {
                                        this.review_action = ReviewStatus::ChangesRequested;
                                        cx.notify();
                                    })
                                    .ok();
                            }
                        },
                    )
                }))
            })
    }
}

impl Render for ReviewView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pr_number = self.selected_pr.number;
        let pr_title = self.selected_pr.title.clone();
        let pr_author = self.selected_pr.author.clone();
        let file_entries = self.collect_file_entries();
        let file_count = file_entries.len();

        let mut file_comments: HashMap<SharedString, Vec<ReviewComment>> = HashMap::default();
        let mut general_comments: Vec<ReviewComment> = Vec::new();
        for comment in &self.pr_comments {
            if let Some(path) = &comment.path {
                file_comments
                    .entry(path.clone())
                    .or_default()
                    .push(comment.clone());
            } else {
                general_comments.push(comment.clone());
            }
        }

        let mut scrollable = v_flex()
            .id("review-scrollable")
            .flex_1()
            .overflow_scroll()
            .gap_1();

        for (ix, entry) in self.display_entries.iter().enumerate() {
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
                    let was_expanded = *expanded;

                    scrollable = scrollable.child(
                        h_flex()
                            .id(SharedString::from(format!("rv_dir_{}", ix)))
                            .px_2()
                            .py_1()
                            .gap_2()
                            .rounded_md()
                            .cursor_pointer()
                            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
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
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                if was_expanded {
                                    this.expanded_dirs.remove(&dir_path);
                                } else {
                                    this.expanded_dirs.insert(dir_path.clone());
                                }
                                this.rebuild_display_entries();
                                cx.notify();
                            })),
                    );
                }
                DisplayEntry::File {
                    entry_index,
                    depth,
                    display_name,
                } => {
                    let Some((path, status, additions, deletions)) =
                        file_entries.get(*entry_index)
                    else {
                        continue;
                    };

                    let (icon, color) = match status {
                        Some(FileChangeStatus::Added) => (IconName::Plus, Color::Created),
                        Some(FileChangeStatus::Modified) => (IconName::Pencil, Color::Modified),
                        Some(FileChangeStatus::Deleted) => (IconName::Dash, Color::Deleted),
                        Some(FileChangeStatus::Renamed { .. }) => {
                            (IconName::ArrowRight, Color::Modified)
                        }
                        None => (IconName::File, Color::Muted),
                    };

                    let indent = *depth as f32 * TREE_INDENT + 8.0;
                    let repo_path = RepoPath::new(path.as_ref()).ok();
                    let comment_count = file_comments
                        .get(path)
                        .map(|c| c.len())
                        .unwrap_or(0);
                    let comments_expanded =
                        self.expanded_comment_files.contains(path);
                    let path_for_toggle = path.clone();

                    let file_row = h_flex()
                        .id(SharedString::from(format!("pr_file_{}", ix)))
                        .px_2()
                        .py_1()
                        .gap_2()
                        .rounded_md()
                        .cursor_pointer()
                        .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
                        .pl(px(indent))
                        .child(Icon::new(icon).size(IconSize::Small).color(color))
                        .child(
                            h_flex()
                                .flex_1()
                                .overflow_x_hidden()
                                .gap_2()
                                .child(
                                    Label::new(display_name.to_string())
                                        .size(LabelSize::Small)
                                        .single_line(),
                                )
                                .when(*additions > 0, |el| {
                                    el.child(
                                        Label::new(format!("+{}", additions))
                                            .size(LabelSize::XSmall)
                                            .color(Color::Created),
                                    )
                                })
                                .when(*deletions > 0, |el| {
                                    el.child(
                                        Label::new(format!("-{}", deletions))
                                            .size(LabelSize::XSmall)
                                            .color(Color::Deleted),
                                    )
                                }),
                        )
                        .when(comment_count > 0, |row| {
                            let chevron = if comments_expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            };
                            row.child(
                                h_flex()
                                    .id(SharedString::from(format!("comment_badge_{}", ix)))
                                    .flex_none()
                                    .gap_1()
                                    .px_1()
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .hover(|style| style.bg(cx.theme().colors().element_hover))
                                    .child(
                                        Icon::new(IconName::Chat)
                                            .size(IconSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                    .child(
                                        Label::new(comment_count.to_string())
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                    .child(
                                        Icon::new(chevron)
                                            .size(IconSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                    .on_click(cx.listener(
                                        move |this, _event, _window, cx| {
                                            if this
                                                .expanded_comment_files
                                                .contains(&path_for_toggle)
                                            {
                                                this.expanded_comment_files
                                                    .remove(&path_for_toggle);
                                            } else {
                                                this.expanded_comment_files
                                                    .insert(path_for_toggle.clone());
                                            }
                                            cx.notify();
                                        },
                                    )),
                            )
                        });

                    let file_row = if let Some(repo_path) = repo_path {
                        file_row.on_click(cx.listener(move |_this, _event, _window, cx| {
                            cx.emit(ReviewViewEvent::OpenFileDiff(repo_path.clone()));
                        }))
                    } else {
                        file_row
                    };

                    scrollable = scrollable.child(file_row);

                    if comments_expanded {
                        if let Some(comments) = file_comments.get(path) {
                            for (comment_ix, comment) in comments.iter().enumerate() {
                                let is_reply = comment.reply_to.is_some();
                                let line_label = comment
                                    .line
                                    .map(|l| format!("at {} ", l))
                                    .unwrap_or_default();
                                let body_preview: String = comment
                                    .body
                                    .chars()
                                    .take(60)
                                    .collect::<String>()
                                    .lines()
                                    .next()
                                    .unwrap_or("")
                                    .to_string();
                                let comment_repo_path =
                                    RepoPath::new(path.as_ref()).ok();

                                let row = h_flex()
                                    .id(SharedString::from(format!(
                                        "compact_comment_{}_{}", ix, comment_ix
                                    )))
                                    .px_2()
                                    .py_0p5()
                                    .gap_1()
                                    .pl(px(indent + 16.0))
                                    .when(is_reply, |el| el.pl(px(indent + 32.0)))
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .hover(|style| {
                                        style.bg(cx.theme().colors().ghost_element_hover)
                                    })
                                    .when(!line_label.is_empty(), |el| {
                                        el.child(
                                            Label::new(line_label)
                                                .size(LabelSize::XSmall)
                                                .color(Color::Accent),
                                        )
                                    })
                                    .child(
                                        Label::new(format!("@{}", comment.author))
                                            .size(LabelSize::XSmall)
                                            .color(Color::Default),
                                    )
                                    .child(
                                        div().overflow_x_hidden().flex_1().child(
                                            Label::new(body_preview)
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted)
                                                .single_line(),
                                        ),
                                    );

                                let row = if let Some(ref repo_path) = comment_repo_path {
                                    let repo_path = repo_path.clone();
                                    row.on_click(cx.listener(
                                        move |_this, _event, _window, cx| {
                                            cx.emit(ReviewViewEvent::OpenFileDiff(
                                                repo_path.clone(),
                                            ));
                                        },
                                    ))
                                } else {
                                    row
                                };

                                scrollable = scrollable.child(row);
                            }
                        }
                    }
                }
            }
        }

        if !general_comments.is_empty() {
            scrollable = scrollable.child(
                h_flex().px_2().pt_2().child(
                    Label::new(format!("General comments ({})", general_comments.len()))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            );
            for comment in &general_comments {
                scrollable = scrollable.child(CommentCard::new(comment.clone()));
            }
        }

        if self.pr_comments_loading {
            scrollable = scrollable.child(
                h_flex().px_2().py_1().child(
                    Label::new("Loading comments...")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            );
        }

        v_flex()
            .id("review-thread")
            .size_full()
            .child(
                h_flex()
                    .flex_none()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .items_center()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        IconButton::new("back-to-pr-list", IconName::ArrowLeft)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Back to PR list"))
                            .on_click(cx.listener(|_this, _, _window, cx| {
                                cx.emit(ReviewViewEvent::Back);
                            })),
                    )
                    .child(
                        Label::new(format!("#{}", pr_number))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        div().overflow_x_hidden().flex_1().child(
                            Label::new(pr_title.to_string())
                                .size(LabelSize::Small)
                                .single_line(),
                        ),
                    )
                    .child(
                        Label::new(format!("by {} · {} files", pr_author, file_count))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .child(scrollable)
            .child(
                v_flex()
                    .flex_none()
                    .border_t_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().editor_background)
                    .child(
                        div()
                            .id("comment-editor-container")
                            .px_2()
                            .pt_2()
                            .w_full()
                            .cursor_text()
                            .on_click(cx.listener(|this, _, window, cx| {
                                window.focus(&this.comment_editor.focus_handle(cx), cx);
                            }))
                            .child(self.comment_editor.clone()),
                    )
                    .child(
                        h_flex()
                            .px_2()
                            .py_1()
                            .justify_end()
                            .child(self.render_review_action_button(cx)),
                    ),
            )
    }
}
