use crate::review_provider::{
    CheckRollup, PullRequestInfo, PullRequestState, ReviewProvider, ReviewStatus,
};
use editor::{Editor, EditorEvent};
use gpui::{
    Anchor, AnyElement, Context, Entity, EventEmitter, Render, SharedString,
    UniformListScrollHandle, Window, px, uniform_list,
};
use std::sync::Arc;
use ui::{
    Avatar, Button, ButtonSize, ButtonStyle, Color, CommonAnimationExt, ContextMenu, Facepile,
    Icon, IconButton, IconName, IconSize, IntoElement, Label, LabelSize, PopoverMenuHandle,
    TintColor, Tooltip, div, h_flex, prelude::*, v_flex,
};
use ui::PopoverMenu;

pub enum PullRequestListEvent {
    Selected(PullRequestInfo),
}

/// Tracks resolution of the GitHub provider so the list can distinguish
/// "still figuring out the remote" from "this repo has no GitHub remote".
#[derive(Clone, Copy, PartialEq)]
pub enum RemoteState {
    /// Provider resolution is in flight; show a loader.
    Resolving,
    /// No GitHub remote could be resolved; show the empty-state hint.
    Unavailable,
    /// Provider is ready; show the PR list.
    Ready,
}

/// Fixed (two-line) row height so the PR list can be virtualized.
const ROW_HEIGHT: f32 = 44.0;

pub struct PullRequestList {
    provider: Option<Arc<dyn ReviewProvider>>,
    remote_owner: Option<String>,
    remote_repo: Option<String>,
    remote_state: RemoteState,
    pull_requests: Vec<PullRequestInfo>,
    /// `pull_requests` filtered by the current search query — cached so `render`
    /// does no per-frame filtering.
    filtered: Vec<PullRequestInfo>,
    loading: bool,
    /// Last load failure (e.g. a GitHub API error), surfaced in place of the
    /// spinner so the panel doesn't appear stuck.
    error: Option<SharedString>,
    /// True while a follow-up page is being fetched (infinite scroll).
    loading_more: bool,
    /// Cursor for the next page; None once the last page has loaded.
    end_cursor: Option<String>,
    has_next_page: bool,
    /// Repository-wide count for the current filter, shown in the header
    /// regardless of how many pages have been loaded so far.
    total_count: usize,
    /// When false, draft PRs are hidden from the rendered list (client-side;
    /// the GraphQL connection has no draft argument).
    show_drafts: bool,
    filter: PullRequestState,
    filter_menu_handle: PopoverMenuHandle<ContextMenu>,
    search_editor: Entity<Editor>,
    scroll_handle: UniformListScrollHandle,
}

impl EventEmitter<PullRequestListEvent> for PullRequestList {}

impl PullRequestList {
    pub fn new(
        provider: Option<Arc<dyn ReviewProvider>>,
        remote_owner: Option<String>,
        remote_repo: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Filter by # or author...", window, cx);
            editor
        });

        cx.subscribe_in(&search_editor, window, |this, _editor, event: &EditorEvent, _window, cx| {
            if matches!(event, EditorEvent::BufferEdited { .. }) {
                this.recompute_filtered(cx);
                cx.notify();
            }
        })
        .detach();

        let remote_state = if provider.is_some() {
            RemoteState::Ready
        } else {
            RemoteState::Resolving
        };

        Self {
            provider,
            remote_owner,
            remote_repo,
            remote_state,
            pull_requests: Vec::new(),
            filtered: Vec::new(),
            loading: false,
            error: None,
            loading_more: false,
            end_cursor: None,
            has_next_page: false,
            total_count: 0,
            show_drafts: false,
            filter: PullRequestState::Open,
            filter_menu_handle: PopoverMenuHandle::default(),
            search_editor,
            scroll_handle: UniformListScrollHandle::new(),
        }
    }

    fn recompute_filtered(&mut self, cx: &mut Context<Self>) {
        let query = self.search_editor.read(cx).text(cx).to_lowercase();
        self.filtered = self
            .pull_requests
            .iter()
            .filter(|pr| {
                if !self.show_drafts && pr.is_draft {
                    return false;
                }
                if query.is_empty() {
                    return true;
                }
                let query_trimmed = query.trim_start_matches('#');
                pr.number.to_string().contains(query_trimmed)
                    || pr.author.to_lowercase().contains(&query)
                    || pr.title.to_lowercase().contains(&query)
            })
            .cloned()
            .collect();
    }

    pub fn set_provider(
        &mut self,
        provider: Arc<dyn ReviewProvider>,
        owner: String,
        repo: String,
        cx: &mut Context<Self>,
    ) {
        self.provider = Some(provider);
        self.remote_owner = Some(owner);
        self.remote_repo = Some(repo);
        self.remote_state = RemoteState::Ready;
        self.load_pull_requests(cx);
    }

    pub fn set_remote_state(&mut self, state: RemoteState, cx: &mut Context<Self>) {
        if self.remote_state != state {
            self.remote_state = state;
            cx.notify();
        }
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.load_pull_requests(cx);
    }

    pub fn load_if_empty(&mut self, cx: &mut Context<Self>) {
        if self.pull_requests.is_empty() {
            self.load_pull_requests(cx);
        }
    }

    fn load_pull_requests(&mut self, cx: &mut Context<Self>) {
        let Some(provider) = self.provider.clone() else {
            return;
        };
        let Some(owner) = self.remote_owner.clone() else {
            return;
        };
        let Some(repo) = self.remote_repo.clone() else {
            return;
        };

        let state = self.filter.clone();
        self.loading = true;
        self.error = None;
        self.loading_more = false;
        self.end_cursor = None;
        self.has_next_page = false;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = provider.fetch_pull_requests(&owner, &repo, state, None).await;
            this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok(page) => {
                        this.error = None;
                        this.pull_requests = page.pull_requests;
                        this.total_count = page.total_count;
                        this.end_cursor = page.end_cursor;
                        this.has_next_page = page.has_next_page;
                        this.recompute_filtered(cx);
                        this.maybe_load_more_for_fill(cx);
                    }
                    Err(error) => {
                        this.error = Some(format_load_error(&error));
                    }
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    /// Fetch the next page and append it. No-op while a load is already in
    /// flight or there are no more pages.
    fn load_more_pull_requests(&mut self, cx: &mut Context<Self>) {
        if self.loading || self.loading_more || !self.has_next_page {
            return;
        }
        let (Some(provider), Some(owner), Some(repo)) = (
            self.provider.clone(),
            self.remote_owner.clone(),
            self.remote_repo.clone(),
        ) else {
            return;
        };

        let state = self.filter.clone();
        let after = self.end_cursor.clone();
        self.loading_more = true;

        cx.spawn(async move |this, cx| {
            let page = provider
                .fetch_pull_requests(&owner, &repo, state, after)
                .await?;
            this.update(cx, |this, cx| {
                this.pull_requests.extend(page.pull_requests);
                this.total_count = page.total_count;
                this.end_cursor = page.end_cursor;
                this.has_next_page = page.has_next_page;
                this.loading_more = false;
                this.recompute_filtered(cx);
                this.maybe_load_more_for_fill(cx);
                cx.notify();
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// When a client-side filter (drafts hidden, search) leaves too few visible
    /// rows to scroll, the infinite-scroll trigger can't fire, so eagerly pull
    /// more pages until the viewport can fill or the list is exhausted.
    fn maybe_load_more_for_fill(&mut self, cx: &mut Context<Self>) {
        const MIN_VISIBLE_ROWS: usize = 20;
        if self.has_next_page && !self.loading_more && self.filtered.len() < MIN_VISIBLE_ROWS {
            self.load_more_pull_requests(cx);
        }
    }

    fn set_filter(&mut self, state: PullRequestState, cx: &mut Context<Self>) {
        if self.filter != state {
            self.filter = state;
            self.load_pull_requests(cx);
        }
    }
}

impl PullRequestList {
    fn render_row(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let pr = self.filtered[ix].clone();
        let number = pr.number;
        let title = pr.title.clone();
        let author = pr.author.clone();
        let updated = pr.updated_at.clone();
        let is_draft = pr.is_draft;
        let comment_count = pr.comment_count;
        let participants = pr.participants.clone();

        let checks_icon = pr.checks.map(|rollup| {
            let (icon, color, tip) = match rollup {
                CheckRollup::Success => (IconName::Check, Color::Created, "Checks passed"),
                CheckRollup::Failure => (IconName::XCircle, Color::Error, "Checks failing"),
                CheckRollup::Pending => (IconName::TodoProgress, Color::Warning, "Checks running"),
            };
            div()
                .id(("pr-checks", number as usize))
                .tooltip(Tooltip::text(tip))
                .child(Icon::new(icon).size(IconSize::XSmall).color(color))
        });
        let review_icon: Option<(IconName, Color, SharedString)> = match pr.review_status {
            ReviewStatus::Approved => Some((IconName::ThumbsUp, Color::Created, "Approved".into())),
            ReviewStatus::ChangesRequested => {
                Some((IconName::ThumbsDown, Color::Error, "Changes requested".into()))
            }
            // Partially approved: at least one approval, but more are required.
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
        let merge_icon = pr.mergeable.map(|m| {
            if m {
                (IconName::GitBranch, Color::Created, "Mergeable")
            } else {
                (IconName::GitMergeConflict, Color::Error, "Merge conflict")
            }
        });
        // Compact approval count, shown whenever approvals are required.
        let approval_label = pr
            .required_approvals
            .map(|required| SharedString::from(format!("{}/{}", pr.approvals, required)));
        let extra_labels = pr.labels.len().saturating_sub(4);
        let label_pills: Vec<(SharedString, gpui::Hsla)> = pr
            .labels
            .iter()
            .take(4)
            .map(|l| (l.name.clone(), label_hsla(&l.color)))
            .collect();

        let reviewer_facepile = (!participants.is_empty()).then(|| {
            div()
                .id(("pr-reviewers", number as usize))
                .flex_none()
                .tooltip(Tooltip::text(format!(
                    "Participants: {}",
                    participants.join(", ")
                )))
                .child(Facepile::new(
                    participants
                        .iter()
                        .map(|login| {
                            Avatar::new(crate::review_view::avatar_url(login))
                                .size(px(14.0))
                                .into_any_element()
                        })
                        .collect(),
                ))
        });
        let status_icons = h_flex()
            .flex_none()
            .gap_1()
            .items_center()
            .children(checks_icon)
            .children(merge_icon.map(|(icon, color, tip)| {
                div()
                    .id(("pr-merge", number as usize))
                    .tooltip(Tooltip::text(tip))
                    .child(Icon::new(icon).size(IconSize::XSmall).color(color))
            }))
            .children(review_icon.map(|(icon, color, tip)| {
                div()
                    .id(("pr-review", number as usize))
                    .tooltip(Tooltip::text(tip))
                    .child(Icon::new(icon).size(IconSize::XSmall).color(color))
            }))
            .children(
                approval_label
                    .map(|label| Label::new(label).size(LabelSize::XSmall).color(Color::Muted)),
            )
            .when(comment_count > 0, |row| {
                row.child(
                    div()
                        .id(("pr-comments", number as usize))
                        .flex_none()
                        .tooltip(Tooltip::text(format!(
                            "{comment_count} comment{}",
                            if comment_count == 1 { "" } else { "s" }
                        )))
                        .child(
                            h_flex()
                                .gap_0p5()
                                .items_center()
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
                        ),
                )
            })
            .children(reviewer_facepile);

        h_flex()
            .id(SharedString::from(format!("pr_{}", number)))
            .px_2()
            .h(px(ROW_HEIGHT))
            .items_center()
            .gap_2()
            .rounded_md()
            .cursor_pointer()
            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
            .child(Avatar::new(crate::review_view::avatar_url(&author)).size(px(18.0)))
            .child(
                v_flex()
                    .flex_1()
                    .overflow_x_hidden()
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .overflow_x_hidden()
                            .when(is_draft, |line| {
                                line.child(
                                    div()
                                        .flex_none()
                                        .px_1()
                                        .rounded_sm()
                                        .border_1()
                                        .border_color(cx.theme().colors().border)
                                        .bg(cx.theme().colors().element_background)
                                        .child(
                                            Label::new("Draft")
                                                .size(LabelSize::XSmall)
                                                .color(Color::Accent),
                                        ),
                                )
                            })
                            .child(status_icons)
                            .child(
                                Label::new(title.to_string())
                                    .size(LabelSize::Small)
                                    .single_line(),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .overflow_x_hidden()
                            .child(
                                Label::new(format!("#{} · {} ·", number, author))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .single_line(),
                            )
                            .child(
                                div()
                                    .id(("pr-updated", number as usize))
                                    .flex_none()
                                    .tooltip(Tooltip::text(updated.clone()))
                                    .child(
                                        Label::new(crate::review_view::format_pr_date(&updated))
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    ),
                            )
                            .children(label_pills.into_iter().map(|(name, color)| {
                                div()
                                    .flex_none()
                                    .px_1()
                                    .rounded_sm()
                                    .bg(color.opacity(0.15))
                                    .text_size(px(10.0))
                                    .text_color(color)
                                    .child(name)
                            }))
                            .when(extra_labels > 0, |row| {
                                row.child(
                                    Label::new(format!("+{extra_labels}"))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                            }),
                    ),
            )
            .on_click(cx.listener(move |_this, _event, _window, cx| {
                cx.emit(PullRequestListEvent::Selected(pr.clone()));
            }))
            .into_any_element()
    }
}

/// Turn a fetch error into a concise, user-facing message. GitHub's raw error
/// bodies are JSON blobs, so we special-case the common ones (rate limit, auth)
/// and otherwise show a trimmed version of the message.
fn format_load_error(error: &anyhow::Error) -> SharedString {
    let text = error.to_string();
    let lowercase = text.to_lowercase();
    if lowercase.contains("rate limit") {
        if lowercase.contains("authenticated requests") || lowercase.contains("for ") {
            return "GitHub API rate limit exceeded. Sign in with a GitHub token \
                (GITHUB_TOKEN or `gh auth login`) for a higher limit, then retry."
                .into();
        }
        return "GitHub API rate limit exceeded. Try again later.".into();
    }
    if lowercase.contains("401") || lowercase.contains("bad credentials") {
        return "GitHub authentication failed. Check your token and retry.".into();
    }
    // Fall back to the first line, capped so a giant JSON body doesn't fill the panel.
    let first_line = text.lines().next().unwrap_or(&text).trim();
    let trimmed: String = first_line.chars().take(200).collect();
    SharedString::from(trimmed)
}

/// Parse a GitHub label hex color (e.g. "1d76db") into an Hsla, falling back to
/// a neutral gray.
fn label_hsla(hex: &str) -> gpui::Hsla {
    u32::from_str_radix(hex.trim_start_matches('#'), 16)
        .map(|rgb| gpui::rgb(rgb).into())
        .unwrap_or_else(|_| gpui::rgb(0x8888_88).into())
}

impl Render for PullRequestList {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.remote_state == RemoteState::Resolving {
            return v_flex()
                .size_full()
                .justify_center()
                .items_center()
                .gap_2()
                .child(
                    Icon::new(IconName::ArrowCircle)
                        .size(IconSize::Small)
                        .color(Color::Muted)
                        .with_rotate_animation(2),
                )
                .child(Label::new("Resolving GitHub repository…").color(Color::Muted))
                .into_any_element();
        }

        if self.remote_state == RemoteState::Unavailable || self.provider.is_none() {
            return v_flex()
                .size_full()
                .justify_center()
                .items_center()
                .gap_2()
                .child(Label::new("No GitHub remote detected").color(Color::Muted))
                .child(
                    Label::new("Push to a GitHub remote to see PRs")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .into_any_element();
        }

        if let Some(error) = self.error.clone()
            && self.pull_requests.is_empty()
        {
            return v_flex()
                .size_full()
                .justify_center()
                .items_center()
                .gap_2()
                .px_4()
                .child(
                    Icon::new(IconName::Warning)
                        .size(IconSize::Medium)
                        .color(Color::Error),
                )
                .child(
                    Label::new("Couldn't load pull requests")
                        .color(Color::Default),
                )
                .child(
                    div().max_w_full().child(
                        Label::new(error)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                )
                .child(
                    Button::new("pr-list-retry", "Retry")
                        .size(ButtonSize::Compact)
                        .style(ButtonStyle::Tinted(TintColor::Accent))
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.load_pull_requests(cx);
                        })),
                )
                .into_any_element();
        }

        if self.loading && self.pull_requests.is_empty() {
            return v_flex()
                .size_full()
                .justify_center()
                .items_center()
                .gap_2()
                .child(
                    Icon::new(IconName::ArrowCircle)
                        .size(IconSize::Small)
                        .color(Color::Muted)
                        .with_rotate_animation(2),
                )
                .child(Label::new("Loading pull requests…").color(Color::Muted))
                .into_any_element();
        }

        if self.pull_requests.is_empty() {
            return v_flex()
                .size_full()
                .justify_center()
                .items_center()
                .child(Label::new("No pull requests found").color(Color::Muted))
                .into_any_element();
        }

        let filter_label = match &self.filter {
            PullRequestState::Open => "Open",
            PullRequestState::Closed => "Closed",
            PullRequestState::All => "All",
        };
        let filtered_count = self.filtered.len();
        let weak_list = cx.weak_entity();

        v_flex()
            .id("review-pr-list")
            .size_full()
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .items_center()
                    .child(div().flex_1().child(self.search_editor.clone()))
                    .child(
                        IconButton::new("pr-draft-toggle", IconName::Notepad)
                        .icon_size(IconSize::Small)
                        .toggle_state(self.show_drafts)
                        .tooltip(Tooltip::text(if self.show_drafts {
                            "Hide drafts"
                        } else {
                            "Show drafts"
                        }))
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.show_drafts = !this.show_drafts;
                            this.recompute_filtered(cx);
                            this.maybe_load_more_for_fill(cx);
                            cx.notify();
                        })),
                    )
                    .child(
                        PopoverMenu::new("pr-filter-menu")
                            .trigger(
                                IconButton::new("pr-filter-trigger", IconName::Filter)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text(format!("Filter: {}", filter_label))),
                            )
                            .anchor(Anchor::TopRight)
                            .with_handle(self.filter_menu_handle.clone())
                            .menu({
                                move |window, cx| {
                                    let weak_list = weak_list.clone();
                                    Some(ContextMenu::build(
                                        window,
                                        cx,
                                        move |menu, _window, _cx| {
                                            menu.entry("Open", None, {
                                                let weak_list = weak_list.clone();
                                                move |_window, cx| {
                                                    weak_list
                                                        .update(cx, |this, cx| {
                                                            this.set_filter(
                                                                PullRequestState::Open,
                                                                cx,
                                                            );
                                                        })
                                                        .ok();
                                                }
                                            })
                                            .entry("Closed", None, {
                                                let weak_list = weak_list.clone();
                                                move |_window, cx| {
                                                    weak_list
                                                        .update(cx, |this, cx| {
                                                            this.set_filter(
                                                                PullRequestState::Closed,
                                                                cx,
                                                            );
                                                        })
                                                        .ok();
                                                }
                                            })
                                            .entry("All", None, {
                                                move |_window, cx| {
                                                    weak_list
                                                        .update(cx, |this, cx| {
                                                            this.set_filter(
                                                                PullRequestState::All,
                                                                cx,
                                                            );
                                                        })
                                                        .ok();
                                                }
                                            })
                                        },
                                    ))
                                }
                            }),
                    ),
            )
            .child(
                h_flex().px_2().pb_1().child(
                    Label::new(format!(
                        "{} {} pull requests",
                        self.total_count, filter_label
                    ))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
                ),
            )
            .child(
                uniform_list(
                    "pr-list-rows",
                    filtered_count,
                    cx.processor(|this, range: std::ops::Range<usize>, _window, cx| {
                        // Prefetch the next page as the user scrolls near the end.
                        if this.has_next_page
                            && !this.loading_more
                            && range.end + 10 >= this.filtered.len()
                        {
                            this.load_more_pull_requests(cx);
                        }
                        range.map(|ix| this.render_row(ix, cx)).collect()
                    }),
                )
                .flex_1()
                .track_scroll(&self.scroll_handle),
            )
            .into_any_element()
    }
}
