use crate::comment_card::{CommentCard, CommentPreview};
use crate::file_list::{DisplayEntry, ViewMode, build_file_tree, flatten_file_tree};
use crate::review_provider::{
    CheckRollup, CommentReactions, FileChangeStatus, PullRequestFile, PullRequestInfo,
    PullRequestStatus, ReactionContent, ReactionGroup, ReviewComment, ReviewProvider, ReviewStatus,
};
use collections::{HashMap, HashSet};
use editor::Editor;
use git::repository::RepoPath;
use git::status::{TreeDiff, TreeDiffStatus};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use gpui::{
    Anchor, AnyElement, Context, Entity, EventEmitter, Focusable, ListAlignment, ListState, Render,
    SharedString, Window, list, px,
};
use std::sync::Arc;
use ui::{
    Button, ButtonLike, ButtonSize, Checkbox, Color, ContextMenu, ElevationIndex, Icon, IconButton,
    IconName, IconSize, IntoElement, Label, LabelSize, PopoverMenu, PopoverMenuHandle, SplitButton,
    ToggleState, Tooltip, div, h_flex, prelude::*, v_flex,
};

pub enum ReviewViewEvent {
    OpenFileDiff(RepoPath),
    Back,
    /// Loaded comment data changed (reactions merged/toggled); any injected
    /// inline comment blocks should be re-rendered.
    CommentsChanged,
}

/// Apply a single reaction add/remove to a comment's tally in place, keeping
/// the `viewer_reacted` flag and count consistent (used for optimistic UI).
fn apply_reaction_delta(reactions: &mut Vec<ReactionGroup>, content: ReactionContent, add: bool) {
    if let Some(group) = reactions.iter_mut().find(|g| g.content == content) {
        if add && !group.viewer_reacted {
            group.viewer_reacted = true;
            group.count += 1;
        } else if !add && group.viewer_reacted {
            group.viewer_reacted = false;
            group.count = group.count.saturating_sub(1);
        }
    } else if add {
        reactions.push(ReactionGroup {
            content,
            count: 1,
            viewer_reacted: true,
        });
    }
}

const TREE_INDENT: f32 = 16.0;
/// Fixed row height so the review list can be virtualized with `uniform_list`,
/// which requires uniform item heights.
const ROW_HEIGHT: f32 = 28.0;

pub struct ReviewView {
    provider: Option<Arc<dyn ReviewProvider>>,
    remote_owner: Option<String>,
    remote_repo: Option<String>,
    selected_pr: PullRequestInfo,
    /// PR description markdown, fetched lazily (the list query omits the body).
    description: Option<Entity<Markdown>>,
    description_expanded: bool,
    /// Mergeability, CI rollup, and labels, fetched on selection.
    status: Option<PullRequestStatus>,
    pr_comments: Vec<ReviewComment>,
    pr_comments_loading: bool,
    pr_api_files: Vec<PullRequestFile>,
    tree_diff: Option<TreeDiff>,
    comment_editor: Entity<Editor>,
    comment_submitting: bool,
    /// Editor for the inline reply composer; shown under the thread identified
    /// by `replying_to`.
    reply_editor: Entity<Editor>,
    /// Root comment id whose thread currently has an open reply composer.
    replying_to: Option<u64>,
    reply_submitting: bool,
    review_action: ReviewStatus,
    review_action_menu_handle: PopoverMenuHandle<ContextMenu>,
    view_mode: ViewMode,
    expanded_dirs: HashSet<SharedString>,
    display_entries: Vec<DisplayEntry>,
    /// General (conversation) comment ids that are expanded to their full body;
    /// collapsed comments show only a first-line preview.
    expanded_comments: HashSet<u64>,
    /// File paths the user has marked viewed (synced with GitHub).
    viewed_files: HashSet<SharedString>,
    // Files panel (top) and comments panel (bottom) are separate virtualized
    // lists with their own scroll state.
    file_list_state: ListState,
    comment_list_state: ListState,
    // Cached per-data-change so `render` does no per-frame recompute.
    file_entries: Vec<(SharedString, Option<FileChangeStatus>, u32, u32)>,
    file_comments: HashMap<SharedString, Vec<ReviewComment>>,
    general_comments: Vec<ReviewComment>,
    file_rows: Vec<RowKind>,
    comment_rows: Vec<RowKind>,
}

/// One flattened, virtualizable row in the review scroll area.
#[derive(Clone)]
enum RowKind {
    Directory {
        path: SharedString,
        name: SharedString,
        depth: usize,
        expanded: bool,
    },
    File {
        entry_index: usize,
        depth: usize,
        display_name: SharedString,
        path: SharedString,
        comment_count: usize,
        viewed: bool,
    },
    Comment {
        comment: ReviewComment,
        /// Full-body markdown, built only when the thread is expanded to avoid
        /// holding parsed markdown for every (collapsed) comment.
        body: Option<Entity<Markdown>>,
        /// Markdown for the one-line collapsed preview.
        preview: Entity<Markdown>,
        depth: usize,
        expanded: bool,
    },
    /// Header above a file's comment threads in the comments panel.
    CommentFileHeader {
        display_name: SharedString,
        path: SharedString,
        count: usize,
    },
    GeneralHeader {
        count: usize,
    },
    GeneralComment {
        comment: ReviewComment,
        body: Option<Entity<Markdown>>,
        preview: Entity<Markdown>,
        expanded: bool,
    },
    Loading,
}

impl EventEmitter<ReviewViewEvent> for ReviewView {}

/// Parse a GitHub label hex color (e.g. "1d76db") into a UI color.
fn label_color(hex: &str) -> Color {
    u32::from_str_radix(hex.trim_start_matches('#'), 16)
        .map(|rgb| Color::Custom(gpui::rgb(rgb).into()))
        .unwrap_or(Color::Muted)
}

/// Format a GitHub ISO-8601 timestamp as a relative string (e.g. "3 days ago"),
/// matching git blame's relative timestamps. Falls back to the raw string.
fn format_pr_date(iso: &str) -> String {
    use time::format_description::well_known::Rfc3339;
    match time::OffsetDateTime::parse(iso, &Rfc3339) {
        Ok(timestamp) => {
            let offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
            time_format::format_localized_timestamp(
                timestamp,
                time::OffsetDateTime::now_utc(),
                offset,
                time_format::TimestampFormat::Relative,
            )
        }
        Err(_) => iso.to_string(),
    }
}

/// Markdown source for a collapsed comment's one-line preview: the first
/// non-empty line with leading block markers (heading/quote/list) stripped so
/// it renders as a plain paragraph, but inline emphasis/code kept so the
/// preview is styled rather than literal.
fn preview_source(body: &str) -> String {
    body.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim_start_matches(['#', '>', '-', '*', ' '])
        .chars()
        .take(160)
        .collect()
}

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

        let reply_editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(2, 6, window, cx);
            editor.set_placeholder_text("Reply…", window, cx);
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
            description: None,
            description_expanded: false,
            status: None,
            pr_comments: Vec::new(),
            pr_comments_loading: false,
            pr_api_files: Vec::new(),
            tree_diff: None,
            comment_editor,
            comment_submitting: false,
            reply_editor,
            replying_to: None,
            reply_submitting: false,
            review_action: ReviewStatus::Commented,
            review_action_menu_handle: PopoverMenuHandle::default(),
            view_mode: ViewMode::Flat,
            expanded_dirs: HashSet::default(),
            display_entries: Vec::new(),
            expanded_comments: HashSet::default(),
            viewed_files: HashSet::default(),
            file_list_state: ListState::new(0, ListAlignment::Top, px(1024.0)),
            comment_list_state: ListState::new(0, ListAlignment::Top, px(1024.0)),
            file_entries: Vec::new(),
            file_comments: HashMap::default(),
            general_comments: Vec::new(),
            file_rows: Vec::new(),
            comment_rows: Vec::new(),
        };
        this.load_pr_comments(pr_number, cx);
        this.load_pr_api_files(pr_number, cx);
        this.load_pr_body(pr_number, cx);
        this.load_pr_status(pr_number, cx);
        this.load_viewed_files(cx);
        this
    }

    fn load_viewed_files(&mut self, cx: &mut Context<Self>) {
        let Some(provider) = self.provider.clone() else {
            return;
        };
        let node_id = self.selected_pr.node_id.to_string();
        if node_id.is_empty() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let viewed = provider.fetch_viewed_files(&node_id).await?;
            this.update(cx, |this, cx| {
                this.viewed_files = viewed.into_iter().map(SharedString::from).collect();
                this.rebuild(cx);
                cx.notify();
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    pub fn is_file_viewed(&self, path: &str) -> bool {
        self.viewed_files.iter().any(|p| p.as_ref() == path)
    }

    pub fn toggle_file_viewed(&mut self, path: SharedString, cx: &mut Context<Self>) {
        let viewed = !self.viewed_files.contains(&path);
        if viewed {
            self.viewed_files.insert(path.clone());
        } else {
            self.viewed_files.remove(&path);
        }
        self.rebuild(cx);
        cx.notify();

        let (Some(provider), Some(node_id)) = (
            self.provider.clone(),
            Some(self.selected_pr.node_id.to_string()).filter(|id| !id.is_empty()),
        ) else {
            return;
        };
        cx.spawn(async move |_this, _cx| {
            provider.mark_file_viewed(&node_id, &path, viewed).await?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn load_pr_status(&mut self, pr_number: u32, cx: &mut Context<Self>) {
        let (Some(provider), Some(owner), Some(repo)) = (
            self.provider.clone(),
            self.remote_owner.clone(),
            self.remote_repo.clone(),
        ) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let status = provider
                .fetch_pull_request_status(&owner, &repo, pr_number)
                .await?;
            this.update(cx, |this, cx| {
                this.status = Some(status);
                cx.notify();
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn load_pr_body(&mut self, pr_number: u32, cx: &mut Context<Self>) {
        let (Some(provider), Some(owner), Some(repo)) = (
            self.provider.clone(),
            self.remote_owner.clone(),
            self.remote_repo.clone(),
        ) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let body = provider
                .fetch_pull_request_body(&owner, &repo, pr_number)
                .await?;
            this.update(cx, |this, cx| {
                if !body.trim().is_empty() {
                    this.description =
                        Some(crate::inline_comment::comment_markdown(body.into(), cx));
                    cx.notify();
                }
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    pub fn set_tree_diff(&mut self, tree_diff: Option<&TreeDiff>, cx: &mut Context<Self>) {
        self.tree_diff = tree_diff.map(|td| TreeDiff {
            entries: td.entries.clone(),
        });
        self.rebuild(cx);
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
        self.rebuild(cx);
        cx.notify();
    }

    // Mutators: every change to data or expansion state goes through one of
    // these so `rebuild()` can never be forgotten. Do NOT write the underlying
    // fields directly elsewhere.

    fn begin_loading_comments(&mut self, cx: &mut Context<Self>) {
        self.pr_comments_loading = true;
        self.rebuild(cx);
        cx.notify();
    }

    fn set_pr_comments(&mut self, comments: Vec<ReviewComment>, cx: &mut Context<Self>) {
        self.pr_comments = comments;
        self.pr_comments_loading = false;
        self.rebuild(cx);
        cx.notify();
    }

    fn set_pr_api_files(&mut self, files: Vec<PullRequestFile>, cx: &mut Context<Self>) {
        self.pr_api_files = files;
        self.rebuild(cx);
        cx.notify();
    }

    fn toggle_directory(&mut self, path: SharedString, cx: &mut Context<Self>) {
        if !self.expanded_dirs.remove(&path) {
            self.expanded_dirs.insert(path);
        }
        self.rebuild(cx);
        cx.notify();
    }

    fn toggle_comment(&mut self, comment_id: u64, cx: &mut Context<Self>) {
        if !self.expanded_comments.remove(&comment_id) {
            self.expanded_comments.insert(comment_id);
        }
        self.rebuild(cx);
        cx.notify();
    }

    fn toggle_description(&mut self, cx: &mut Context<Self>) {
        self.description_expanded = !self.description_expanded;
        cx.notify();
    }

    /// PR state / review-status / dates badges, plus a collapsible description.
    fn render_metadata(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pr = &self.selected_pr;
        let colors = cx.theme().colors().clone();

        // Status is conveyed with the same icon vocabulary as the PR list. "Open"
        // is the default, so only the draft exception is surfaced.
        let review_icon: Option<(IconName, Color, SharedString)> = match pr.review_status {
            ReviewStatus::Approved => Some((IconName::ThumbsUp, Color::Created, "Approved".into())),
            ReviewStatus::ChangesRequested => {
                Some((IconName::ThumbsDown, Color::Error, "Changes requested".into()))
            }
            // Has at least one approval but `reviewDecision` is still
            // REVIEW_REQUIRED — partially approved.
            ReviewStatus::Pending if pr.approvals > 0 => {
                let tip = match pr.required_approvals {
                    Some(required) => format!(
                        "{} of {} approvals · {} more needed",
                        pr.approvals,
                        required,
                        required.saturating_sub(pr.approvals)
                    ),
                    None => format!("{} approval(s) · more required", pr.approvals),
                };
                Some((IconName::ThumbsUp, Color::Warning, tip.into()))
            }
            ReviewStatus::Pending => {
                Some((IconName::Person, Color::Muted, "Awaiting review".into()))
            }
            ReviewStatus::Commented => None,
        };
        // Whenever approvals are required, show the running count (e.g. 1/2);
        // hidden when the branch has no approval requirement.
        let approval_label = pr
            .required_approvals
            .map(|required| SharedString::from(format!("{}/{}", pr.approvals, required)));
        let merge_icon = self.status.as_ref().and_then(|s| s.mergeable).map(|m| {
            if m {
                (IconName::GitBranch, Color::Created, "Mergeable")
            } else {
                (IconName::GitMergeConflict, Color::Error, "Merge conflict")
            }
        });
        let checks_icon = self
            .status
            .as_ref()
            .and_then(|s| s.checks)
            .map(|rollup| match rollup {
                CheckRollup::Success => (IconName::Check, Color::Created, "Checks passed"),
                CheckRollup::Failure => (IconName::XCircle, Color::Error, "Checks failing"),
                CheckRollup::Pending => {
                    (IconName::TodoProgress, Color::Warning, "Checks running")
                }
            });

        let pill = |label: &str, color: Color, border: gpui::Hsla, bg: gpui::Hsla| {
            div()
                .px_1()
                .rounded_sm()
                .border_1()
                .border_color(border)
                .bg(bg)
                .child(Label::new(label.to_string()).size(LabelSize::XSmall).color(color))
        };
        let number = pr.number;
        let status_chip = move |suffix: &str, icon: IconName, color: Color, tip: SharedString| {
            div()
                .id(SharedString::from(format!("pr-meta-{suffix}-{number}")))
                .tooltip(Tooltip::text(tip))
                .child(Icon::new(icon).size(IconSize::XSmall).color(color))
        };

        let dates = format!(
            "opened {} · updated {}",
            format_pr_date(&pr.created_at),
            format_pr_date(&pr.updated_at)
        );

        let mut block = v_flex()
            .flex_none()
            .px_2()
            .py_1()
            .gap_1()
            .border_b_1()
            .border_color(colors.border)
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .flex_wrap()
                    .when(pr.is_draft, |row| {
                        row.child(status_chip(
                            "draft",
                            IconName::Notepad,
                            Color::Muted,
                            "Draft".into(),
                        ))
                    })
                    .children(
                        checks_icon
                            .map(|(icon, color, tip)| status_chip("checks", icon, color, tip.into())),
                    )
                    .children(
                        merge_icon.map(|(icon, color, tip)| status_chip("merge", icon, color, tip.into())),
                    )
                    .children(
                        review_icon
                            .map(|(icon, color, tip)| status_chip("review", icon, color, tip)),
                    )
                    .children(approval_label.map(|label| {
                        Label::new(label).size(LabelSize::XSmall).color(Color::Muted)
                    }))
                    .child(
                        Label::new(dates)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .children(
                        self.status
                            .as_ref()
                            .map(|s| s.labels.clone())
                            .unwrap_or_default()
                            .into_iter()
                            .map(|label| {
                                pill(
                                    label.name.as_ref(),
                                    label_color(&label.color),
                                    colors.border_variant,
                                    colors.element_background,
                                )
                            }),
                    ),
            );

        if let Some(description) = self.description.clone() {
            let expanded = self.description_expanded;
            block = block.child(
                h_flex()
                    .id("pr-description-toggle")
                    .w_full()
                    .gap_1()
                    .items_center()
                    .px_1()
                    .py_0p5()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|style| style.bg(colors.element_hover))
                    .child(
                        Icon::new(if expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                    )
                    .child(
                        Label::new("Description")
                            .size(LabelSize::Small)
                            .color(Color::Default),
                    )
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.toggle_description(cx);
                    })),
            );
            if expanded {
                let mut style = MarkdownStyle::themed(MarkdownFont::Editor, window, cx);
                style.base_text_style.color = colors.text;
                style.base_text_style.font_size = px(11.0).into();
                let heading = |size: f32| gpui::TextStyleRefinement {
                    font_size: Some(px(size).into()),
                    line_height: Some(px(size + 4.0).into()),
                    ..Default::default()
                };
                style.heading_level_styles = Some(markdown::HeadingLevelStyles {
                    h1: Some(heading(16.0)),
                    h2: Some(heading(14.0)),
                    h3: Some(heading(13.0)),
                    h4: Some(heading(12.0)),
                    h5: Some(heading(12.0)),
                    h6: Some(heading(11.0)),
                });
                let max_height = window.viewport_size().height * 0.5;
                block = block.child(
                    div()
                        .id("pr-description-body")
                        .w_full()
                        .min_w_0()
                        .mb_1()
                        .px_2()
                        .pt_2()
                        .pb_4()
                        .rounded_md()
                        .border_1()
                        .border_color(colors.border)
                        .bg(colors.editor_background)
                        .overflow_x_hidden()
                        .max_h(max_height)
                        .overflow_y_scroll()
                        .text_size(px(11.0))
                        .child(MarkdownElement::new(description, style)),
                );
            }
        }
        block
    }

    /// Recompute all cached state (file entries, grouped comments, display
    /// entries, and the flattened `visible_rows`). Private — reach it only via
    /// the mutator methods above so it can't be skipped.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        self.file_entries = self.compute_file_entries();

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
        self.file_comments = file_comments;
        self.general_comments = general_comments;

        let mut display_entries = Vec::new();
        match self.view_mode {
            ViewMode::Flat => {
                for (ix, (path, _, _, _)) in self.file_entries.iter().enumerate() {
                    display_entries.push(DisplayEntry::File {
                        entry_index: ix,
                        depth: 0,
                        display_name: path.clone(),
                    });
                }
            }
            ViewMode::Tree => {
                let paths: Vec<(usize, &str)> = self
                    .file_entries
                    .iter()
                    .enumerate()
                    .map(|(ix, (path, _, _, _))| (ix, path.as_ref()))
                    .collect();
                let tree = build_file_tree(&paths);
                flatten_file_tree(&tree, 0, &self.expanded_dirs, &mut display_entries);
            }
        }
        self.display_entries = display_entries;

        // Top panel: pure file/dir navigation tree (no inline comments).
        let mut file_rows = Vec::new();
        for entry in &self.display_entries {
            match entry {
                DisplayEntry::Directory {
                    path,
                    name,
                    depth,
                    expanded,
                } => file_rows.push(RowKind::Directory {
                    path: path.clone(),
                    name: name.clone(),
                    depth: *depth,
                    expanded: *expanded,
                }),
                DisplayEntry::File {
                    entry_index,
                    depth,
                    display_name,
                } => {
                    let Some((path, _, _, _)) = self.file_entries.get(*entry_index) else {
                        continue;
                    };
                    let path = path.clone();
                    let comment_count = self.file_comments.get(&path).map_or(0, |c| c.len());
                    let viewed = self.viewed_files.contains(&path);
                    file_rows.push(RowKind::File {
                        entry_index: *entry_index,
                        depth: *depth,
                        display_name: display_name.clone(),
                        path,
                        comment_count,
                        viewed,
                    });
                }
            }
        }
        self.file_rows = file_rows;

        // Bottom panel: all comment threads, grouped by file (in tree order),
        // followed by the general conversation comments.
        let mut comment_rows = Vec::new();
        for (path, _, _, _) in &self.file_entries {
            let Some(comments) = self.file_comments.get(path) else {
                continue;
            };
            if comments.is_empty() {
                continue;
            }
            comment_rows.push(RowKind::CommentFileHeader {
                display_name: path.clone(),
                path: path.clone(),
                count: comments.len(),
            });
            for comment in comments {
                // Expansion is per-thread: a reply follows its root's state, so
                // opening the root opens the whole thread.
                let thread_id = comment.reply_to.unwrap_or(comment.id);
                let expanded = self.expanded_comments.contains(&thread_id);
                let body = expanded
                    .then(|| crate::inline_comment::comment_markdown(comment.body.clone(), cx));
                let preview = crate::inline_comment::comment_markdown(
                    preview_source(&comment.body).into(),
                    cx,
                );
                comment_rows.push(RowKind::Comment {
                    comment: comment.clone(),
                    body,
                    preview,
                    depth: 0,
                    expanded,
                });
            }
        }
        if !self.general_comments.is_empty() {
            comment_rows.push(RowKind::GeneralHeader {
                count: self.general_comments.len(),
            });
            for comment in &self.general_comments {
                let expanded = self.expanded_comments.contains(&comment.id);
                let body = expanded
                    .then(|| crate::inline_comment::comment_markdown(comment.body.clone(), cx));
                let preview = crate::inline_comment::comment_markdown(
                    preview_source(&comment.body).into(),
                    cx,
                );
                comment_rows.push(RowKind::GeneralComment {
                    comment: comment.clone(),
                    body,
                    preview,
                    expanded,
                });
            }
        }
        if self.pr_comments_loading {
            comment_rows.push(RowKind::Loading);
        }
        self.comment_rows = comment_rows;

        self.file_list_state.reset(self.file_rows.len());
        self.comment_list_state.reset(self.comment_rows.len());
    }

    fn compute_file_entries(&self) -> Vec<(SharedString, Option<FileChangeStatus>, u32, u32)> {
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

    /// Append a freshly posted comment and rebuild the cached rows so it shows
    /// in the sidebar (and so re-injected inline blocks pick it up).
    pub fn add_comment(&mut self, comment: ReviewComment, cx: &mut Context<Self>) {
        self.pr_comments.push(comment);
        self.rebuild(cx);
        cx.notify();
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

        self.begin_loading_comments(cx);

        cx.spawn(async move |this, cx| {
            let comments = provider.fetch_reviews(&owner, &repo, pr_number).await?;
            this.update(cx, |this, cx| {
                this.set_pr_comments(comments, cx);
            })?;
            // Reactions need a separate GraphQL round-trip; fold them in once the
            // comments are already on screen rather than blocking the first paint.
            let reactions = provider
                .fetch_comment_reactions(&owner, &repo, pr_number)
                .await?;
            this.update(cx, |this, cx| {
                this.merge_reactions(reactions, cx);
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// Fold reaction tallies (and the GraphQL node ids needed to mutate them)
    /// into the already-loaded comments, keyed by numeric comment id.
    fn merge_reactions(&mut self, reactions: Vec<CommentReactions>, cx: &mut Context<Self>) {
        let by_id: HashMap<u64, CommentReactions> = reactions
            .into_iter()
            .map(|reaction| (reaction.database_id, reaction))
            .collect();
        for comment in &mut self.pr_comments {
            if let Some(reaction) = by_id.get(&comment.id) {
                comment.node_id = reaction.node_id.clone();
                comment.reactions = reaction.reactions.clone();
            }
        }
        self.rebuild(cx);
        cx.emit(ReviewViewEvent::CommentsChanged);
        cx.notify();
    }

    /// Add or remove the current user's reaction on a comment, updating the
    /// local tally optimistically and reverting on failure.
    pub fn toggle_reaction(
        &mut self,
        comment_id: u64,
        content: ReactionContent,
        add: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(provider) = self.provider.clone() else {
            return;
        };
        let Some(comment) = self.pr_comments.iter_mut().find(|c| c.id == comment_id) else {
            return;
        };
        let node_id = comment.node_id.clone();
        if node_id.is_empty() {
            log::warn!("toggle_reaction: comment {comment_id} has no node id yet");
            return;
        }
        apply_reaction_delta(&mut comment.reactions, content, add);
        self.rebuild(cx);
        cx.emit(ReviewViewEvent::CommentsChanged);
        cx.notify();

        cx.spawn(async move |this, cx| {
            if let Err(error) = provider.set_reaction(&node_id, content, add).await {
                log::error!("failed to set reaction: {error:#}");
                this.update(cx, |this, cx| {
                    if let Some(comment) = this.pr_comments.iter_mut().find(|c| c.id == comment_id) {
                        // Revert the optimistic toggle.
                        apply_reaction_delta(&mut comment.reactions, content, !add);
                        this.rebuild(cx);
                        this.emit_comments_changed(cx);
                        cx.notify();
                    }
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn emit_comments_changed(&mut self, cx: &mut Context<Self>) {
        cx.emit(ReviewViewEvent::CommentsChanged);
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
                this.set_pr_api_files(files, cx);
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

    fn submit_reply(&mut self, parent_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(provider) = self.provider.clone() else {
            return;
        };
        let Some(owner) = self.remote_owner.clone() else {
            return;
        };
        let Some(repo) = self.remote_repo.clone() else {
            return;
        };

        let body = self.reply_editor.read(cx).text(cx);
        if body.trim().is_empty() {
            return;
        }
        let pr_number = self.selected_pr.number;
        self.reply_submitting = true;
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let new_comment = provider
                .reply_to_comment(&owner, &repo, pr_number, &body, parent_id)
                .await?;
            this.update_in(cx, |this, window, cx| {
                this.pr_comments.push(new_comment);
                this.reply_submitting = false;
                this.replying_to = None;
                this.reply_editor.update(cx, |editor, cx| {
                    editor.clear(window, cx);
                });
                this.rebuild(cx);
                cx.notify();
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn render_reply_composer(&self, parent_id: u64, cx: &mut Context<Self>) -> impl IntoElement {
        let submitting = self.reply_submitting;
        v_flex()
            .mt_1()
            .gap_1()
            .child(
                div()
                    .id("reply-editor-container")
                    .px_2()
                    .pt_1()
                    .w_full()
                    .cursor_text()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .rounded_md()
                    .on_click(cx.listener(|this, _, window, cx| {
                        window.focus(&this.reply_editor.focus_handle(cx), cx);
                    }))
                    .child(self.reply_editor.clone()),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_1()
                    .child(
                        Button::new("reply-cancel", "Cancel")
                            .size(ButtonSize::Compact)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.replying_to = None;
                                this.reply_editor.update(cx, |editor, cx| {
                                    editor.clear(window, cx);
                                });
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("reply-submit", "Reply")
                            .size(ButtonSize::Compact)
                            .label_size(LabelSize::Small)
                            .disabled(submitting)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.submit_reply(parent_id, window, cx);
                            })),
                    ),
            )
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

impl ReviewView {
    /// Render a single cached row. Pure read of cached state — no recompute.
    fn render_row(&mut self, row: RowKind, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        match &row {
            RowKind::Directory {
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
                h_flex()
                    .id(SharedString::from(format!("rv_dir_{}", ix)))
                    .px_2()
                    .h(px(ROW_HEIGHT))
                    .items_center()
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
                        this.toggle_directory(dir_path.clone(), cx);
                    }))
                    .into_any_element()
            }
            RowKind::File {
                entry_index,
                depth,
                display_name,
                path,
                comment_count,
                viewed,
            } => {
                let (status, additions, deletions) = match self.file_entries.get(*entry_index) {
                    Some((_, status, additions, deletions)) => {
                        (status.clone(), *additions, *deletions)
                    }
                    None => (None, 0, 0),
                };
                let (icon, color) = match status {
                    Some(FileChangeStatus::Added) => (IconName::Plus, Color::Created),
                    Some(FileChangeStatus::Modified) => (IconName::Pencil, Color::Modified),
                    Some(FileChangeStatus::Deleted) => (IconName::Dash, Color::Deleted),
                    Some(FileChangeStatus::Renamed) => (IconName::ArrowRight, Color::Modified),
                    None => (IconName::File, Color::Muted),
                };
                let indent = *depth as f32 * TREE_INDENT + 8.0;
                let repo_path = RepoPath::new(path.as_ref()).ok();
                let comment_count = *comment_count;
                let display_name = display_name.clone();
                let is_viewed = *viewed;
                let viewed_path = path.clone();

                let file_row = h_flex()
                    .id(SharedString::from(format!("pr_file_{}", ix)))
                    .px_2()
                    .h(px(ROW_HEIGHT))
                    .items_center()
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
                                    .color(if is_viewed {
                                        Color::Muted
                                    } else {
                                        Color::Default
                                    })
                                    .single_line(),
                            )
                            .when(additions > 0, |el| {
                                el.child(
                                    Label::new(format!("+{}", additions))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Created),
                                )
                            })
                            .when(deletions > 0, |el| {
                                el.child(
                                    Label::new(format!("-{}", deletions))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Deleted),
                                )
                            }),
                    )
                    .when(comment_count > 0, |row| {
                        row.child(
                            h_flex()
                                .flex_none()
                                .gap_1()
                                .px_1()
                                .child(
                                    Icon::new(IconName::Chat)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .child(
                                    Label::new(comment_count.to_string())
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                ),
                        )
                    })
                    .child({
                        let weak = cx.weak_entity();
                        Checkbox::new(
                            SharedString::from(format!("pr_file_viewed_{}", ix)),
                            if is_viewed {
                                ToggleState::Selected
                            } else {
                                ToggleState::Unselected
                            },
                        )
                        .on_click_ext(move |_state, _event, _window, cx| {
                            cx.stop_propagation();
                            let viewed_path = viewed_path.clone();
                            weak.update(cx, |this, cx| this.toggle_file_viewed(viewed_path, cx))
                                .ok();
                        })
                    });

                let file_row = if let Some(repo_path) = repo_path {
                    file_row.on_click(cx.listener(move |_this, _event, _window, cx| {
                        cx.emit(ReviewViewEvent::OpenFileDiff(repo_path.clone()));
                    }))
                } else {
                    file_row
                };
                file_row.into_any_element()
            }
            RowKind::Comment {
                comment,
                body,
                preview,
                depth,
                expanded,
            } => {
                let indent = *depth as f32 * TREE_INDENT + 8.0;
                let comment_id = comment.id;
                let is_root = comment.reply_to.is_none();
                let is_reply = !is_root;
                // Toggling any row in a thread toggles the whole thread.
                let thread_id = comment.reply_to.unwrap_or(comment_id);

                if !*expanded {
                    return h_flex()
                        .id(SharedString::from(format!("comment-{comment_id}")))
                        .pl(px(if is_reply { indent + 16.0 } else { indent }))
                        .pr_2()
                        .py_1()
                        .gap_1()
                        .items_center()
                        .cursor_pointer()
                        .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
                        .child(
                            Icon::new(IconName::ChevronRight)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new(format!("@{}", comment.author))
                                .size(LabelSize::XSmall)
                                .color(Color::Default),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_x_hidden()
                                .child(CommentPreview::new(preview.clone())),
                        )
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.toggle_comment(thread_id, cx);
                        }))
                        .into_any_element();
                }

                let is_replying = self.replying_to == Some(comment_id);
                let mut container = v_flex()
                    .min_w_0()
                    .pl(px(indent))
                    .pr_2()
                    .gap_1()
                    .child(
                        h_flex()
                            .id(SharedString::from(format!("comment-hdr-{comment_id}")))
                            .px_2()
                            .gap_1()
                            .items_center()
                            .cursor_pointer()
                            .child(
                                Icon::new(IconName::ChevronDown)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(format!("@{}", comment.author))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.toggle_comment(thread_id, cx);
                            })),
                    )
                    .child(CommentCard::new(comment.clone(), body.clone().unwrap_or_else(|| preview.clone())));
                if is_root && !is_replying {
                    container = container.child(
                        h_flex().pl_2().child(
                            Button::new(
                                SharedString::from(format!("reply-{comment_id}")),
                                "Reply",
                            )
                            .size(ButtonSize::Compact)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.replying_to = Some(comment_id);
                                window.focus(&this.reply_editor.focus_handle(cx), cx);
                                cx.notify();
                            })),
                        ),
                    );
                }
                if is_replying {
                    container = container.child(self.render_reply_composer(comment_id, cx));
                }
                container.into_any_element()
            }
            RowKind::CommentFileHeader {
                display_name,
                path,
                count,
            } => {
                let repo_path = RepoPath::new(path.as_ref()).ok();
                let row = h_flex()
                    .id(SharedString::from(format!("comment-file-{ix}")))
                    .px_2()
                    .py_1()
                    .gap_1()
                    .items_center()
                    .when(repo_path.is_some(), |el| el.cursor_pointer())
                    .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
                    .child(
                        Icon::new(IconName::File)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        div().overflow_x_hidden().flex_1().child(
                            Label::new(display_name.to_string())
                                .size(LabelSize::XSmall)
                                .color(Color::Default)
                                .single_line(),
                        ),
                    )
                    .child(
                        Label::new(count.to_string())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    );
                if let Some(repo_path) = repo_path {
                    row.on_click(cx.listener(move |_this, _, _window, cx| {
                        cx.emit(ReviewViewEvent::OpenFileDiff(repo_path.clone()));
                    }))
                    .into_any_element()
                } else {
                    row.into_any_element()
                }
            }
            RowKind::GeneralHeader { count } => h_flex()
                .px_2()
                .h(px(ROW_HEIGHT))
                .items_center()
                .child(
                    Label::new(format!("General comments ({})", count))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .into_any_element(),
            RowKind::GeneralComment {
                comment,
                body,
                preview,
                expanded,
            } => {
                let comment_id = comment.id;
                if *expanded {
                    v_flex()
                        .w_full()
                        .min_w_0()
                        .px_2()
                        .child(
                            h_flex()
                                .id(SharedString::from(format!("gc-hdr-{comment_id}")))
                                .px_2()
                                .gap_1()
                                .items_center()
                                .cursor_pointer()
                                .child(
                                    Icon::new(IconName::ChevronDown)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .child(
                                    Label::new(format!("@{}", comment.author))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .on_click(cx.listener(move |this, _, _window, cx| {
                                    this.toggle_comment(comment_id, cx);
                                })),
                        )
                        .child(CommentCard::new(comment.clone(), body.clone().unwrap_or_else(|| preview.clone())))
                        .into_any_element()
                } else {
                    h_flex()
                        .id(SharedString::from(format!("gc-{comment_id}")))
                        .px_2()
                        .py_1()
                        .gap_1()
                        .items_center()
                        .cursor_pointer()
                        .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
                        .child(
                            Icon::new(IconName::ChevronRight)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new(format!("@{}", comment.author))
                                .size(LabelSize::XSmall)
                                .color(Color::Default),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_x_hidden()
                                .child(CommentPreview::new(preview.clone())),
                        )
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.toggle_comment(comment_id, cx);
                        }))
                        .into_any_element()
                }
            }
            RowKind::Loading => h_flex()
                .px_2()
                .h(px(ROW_HEIGHT))
                .items_center()
                .child(
                    Label::new("Loading comments...")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .into_any_element(),
        }
    }
}

impl Render for ReviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pr_number = self.selected_pr.number;
        let pr_title = self.selected_pr.title.clone();
        let pr_author = self.selected_pr.author.clone();
        let file_count = self.file_entries.len();

        let files = list(self.file_list_state.clone(), {
            let weak = cx.weak_entity();
            move |ix, _window, cx| {
                weak.update(cx, |this, cx| {
                    let Some(row) = this.file_rows.get(ix).cloned() else {
                        return gpui::Empty.into_any_element();
                    };
                    this.render_row(row, ix, cx)
                })
                .unwrap_or_else(|_| gpui::Empty.into_any_element())
            }
        })
        .flex_1()
        .w_full();
        let comments = list(self.comment_list_state.clone(), {
            let weak = cx.weak_entity();
            move |ix, _window, cx| {
                weak.update(cx, |this, cx| {
                    let Some(row) = this.comment_rows.get(ix).cloned() else {
                        return gpui::Empty.into_any_element();
                    };
                    this.render_row(row, ix, cx)
                })
                .unwrap_or_else(|_| gpui::Empty.into_any_element())
            }
        })
        .flex_1()
        .w_full();

        let border = cx.theme().colors().border;
        let section_header = move |label: SharedString| {
            h_flex()
                .flex_none()
                .px_2()
                .py_1()
                .border_b_1()
                .border_color(border)
                .child(Label::new(label).size(LabelSize::XSmall).color(Color::Muted))
        };
        let total_files = self.file_entries.len();
        let viewed_files = self
            .file_entries
            .iter()
            .filter(|(path, _, _, _)| self.viewed_files.contains(path))
            .count();
        let files_label = if total_files > 0 {
            SharedString::from(format!("Files — {viewed_files}/{total_files} viewed"))
        } else {
            SharedString::from("Files")
        };
        let files_panel = v_flex()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_x_hidden()
            .child(section_header(files_label))
            .child(files);
        let comments_panel = v_flex()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_x_hidden()
            .border_t_1()
            .border_color(border)
            .child(section_header(SharedString::from("Comments")))
            .child(comments);

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
            .child(self.render_metadata(window, cx))
            .child(files_panel)
            .child(comments_panel)
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
