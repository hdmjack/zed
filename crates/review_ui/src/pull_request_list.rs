use crate::review_provider::{PullRequestInfo, PullRequestState, ReviewProvider};
use editor::{Editor, EditorEvent};
use gpui::{Anchor, Context, Entity, EventEmitter, Render, SharedString, Window};
use std::sync::Arc;
use ui::{
    Color, ContextMenu, IconButton, IconName, IconSize, IntoElement, Label, LabelSize,
    PopoverMenuHandle, Tooltip, div, h_flex, prelude::*, v_flex,
};
use ui::PopoverMenu;

pub enum PullRequestListEvent {
    Selected(PullRequestInfo),
}

pub struct PullRequestList {
    provider: Option<Arc<dyn ReviewProvider>>,
    remote_owner: Option<String>,
    remote_repo: Option<String>,
    pull_requests: Vec<PullRequestInfo>,
    loading: bool,
    filter: PullRequestState,
    filter_menu_handle: PopoverMenuHandle<ContextMenu>,
    search_editor: Entity<Editor>,
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

        cx.subscribe_in(&search_editor, window, |_this, _editor, event: &EditorEvent, _window, cx| {
            if matches!(event, EditorEvent::BufferEdited { .. }) {
                cx.notify();
            }
        })
        .detach();

        Self {
            provider,
            remote_owner,
            remote_repo,
            pull_requests: Vec::new(),
            loading: false,
            filter: PullRequestState::Open,
            filter_menu_handle: PopoverMenuHandle::default(),
            search_editor,
        }
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
        self.load_pull_requests(cx);
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

impl Render for PullRequestList {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.provider.is_none() {
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
                .child(Label::new("Loading pull requests...").color(Color::Muted))
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

        let query = self
            .search_editor
            .read(cx)
            .text(cx)
            
            .to_lowercase();

        let filtered: Vec<_> = self
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

        let filter_label = match &self.filter {
            PullRequestState::Open => "Open",
            PullRequestState::Closed => "Closed",
            PullRequestState::Merged => "Merged",
            PullRequestState::All => "All",
        };
        let weak_list = cx.weak_entity();

        v_flex()
            .id("review-pr-list")
            .size_full()
            .overflow_scroll()
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
                    Label::new(format!("{} {} pull requests", filtered.len(), filter_label))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            )
            .children(filtered.into_iter().map(|pr| {
                let number = pr.number;
                let title = pr.title.clone();
                let author = pr.author.clone();
                let updated = pr.updated_at.clone();

                h_flex()
                    .id(SharedString::from(format!("pr_{}", number)))
                    .px_2()
                    .py_1()
                    .gap_2()
                    .rounded_md()
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
                    .on_click({
                        cx.listener(move |this, _event, _window, cx| {
                            cx.emit(PullRequestListEvent::Selected(pr.clone()));
                            let _ = this;
                        })
                    })
            }))
            .into_any_element()
    }
}
