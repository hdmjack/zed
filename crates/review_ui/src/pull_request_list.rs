use crate::review_provider::{PullRequestInfo, PullRequestState, ReviewProvider};
use editor::{Editor, EditorEvent};
use gpui::{
    Anchor, AnyElement, Context, Entity, EventEmitter, Render, SharedString,
    UniformListScrollHandle, Window, px, uniform_list,
};
use std::sync::Arc;
use ui::{
    Color, CommonAnimationExt, ContextMenu, Icon, IconButton, IconName, IconSize, IntoElement,
    Label, LabelSize, PopoverMenuHandle, Tooltip, div, h_flex, prelude::*, v_flex,
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
        cx.notify();

        cx.spawn(async move |this, cx| {
            let pull_requests = provider.fetch_pull_requests(&owner, &repo, state).await?;
            this.update(cx, |this, cx| {
                this.pull_requests = pull_requests;
                this.loading = false;
                this.recompute_filtered(cx);
                cx.notify();
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
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
        h_flex()
            .id(SharedString::from(format!("pr_{}", number)))
            .px_2()
            .h(px(ROW_HEIGHT))
            .items_center()
            .gap_2()
            .rounded_md()
            .cursor_pointer()
            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
            .child(
                Label::new(format!("#{}", number))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                v_flex()
                    .overflow_x_hidden()
                    .child(
                        Label::new(title.to_string())
                            .size(LabelSize::Small)
                            .single_line(),
                    )
                    .child(
                        Label::new(format!("by {} · {}", author, updated))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .single_line(),
                    ),
            )
            .on_click(cx.listener(move |_this, _event, _window, cx| {
                cx.emit(PullRequestListEvent::Selected(pr.clone()));
            }))
            .into_any_element()
    }
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
            PullRequestState::Merged => "Merged",
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
                    Label::new(format!("{} {} pull requests", filtered_count, filter_label))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            )
            .child(
                uniform_list(
                    "pr-list-rows",
                    filtered_count,
                    cx.processor(|this, range: std::ops::Range<usize>, _window, cx| {
                        range.map(|ix| this.render_row(ix, cx)).collect()
                    }),
                )
                .flex_1()
                .track_scroll(&self.scroll_handle),
            )
            .into_any_element()
    }
}
