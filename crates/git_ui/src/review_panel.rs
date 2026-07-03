use crate::configuration_view::{ConfigurationEvent, ConfigurationView};
use pull_request::GitHubProvider;
use crate::inline_comment::{ApplySuggestion, ReactToComment, SuggestionBlock, comment_markdown, parse_suggestions, render_pr_comment_block};
use crate::pull_request_list::{PullRequestList, PullRequestListEvent, RemoteState};
use crate::review_view::{ReviewView, ReviewViewEvent};
use git_hosting_providers::resolve_github_token;
use pull_request::{PullRequestInfo, ReactionContent, ReviewComment, ReviewProvider};
use markdown::Markdown;
use anyhow::Result;
use collections::HashMap;
use editor::display_map::{BlockContext, BlockPlacement, BlockProperties, BlockStyle, CustomBlockId};
use editor::{Addon, Editor, EditorEvent};
use multi_buffer::ExcerptBoundaryInfo;
use git::repository::RepoPath;
use git::status::{DiffTreeType, TreeDiff};
use gpui::{
    AnyElement, App, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable, Render,
    SharedString, Subscription, WeakEntity, Window,
};
use text::{Point, ToPoint};
use http_client::HttpClient;
use project::{
    Project,
    git_store::{GitStoreEvent, Repository, RepositoryEvent},
};
use std::sync::Arc;
use std::time::Duration;
use ui::{
    Button, ButtonSize, Checkbox, Color, ContextMenu, DynamicSpacing, IconButton, IconName,
    IconSize, IntoElement, KeybindingHint, Label, LabelSize, PopoverMenu, PopoverMenuHandle, Tab,
    ToggleState, Tooltip, h_flex, prelude::*, v_flex,
};
use serde::{Deserialize, Serialize};
use workspace::Workspace;
use zed_actions::review_panel::{AddComment, SubmitComment, ToggleViewed};

/// Persisted review state, restored when the workspace reopens. Only the PR's
/// identity is stored; the rest of `PullRequestInfo` is re-fetched so the head
/// SHA (used for inline comments) is never stale. Persisted as part of the
/// hosting `GitPanel`'s serialized state.
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct SerializedReviewPanel {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) remote_owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) remote_repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) selected_pr_number: Option<u32>,
}

impl SerializedReviewPanel {
    /// Whether there's nothing worth persisting (no PR selected).
    pub(crate) fn is_empty(&self) -> bool {
        self.selected_pr_number.is_none()
    }
}

/// Events emitted by the embedded review view to its hosting `GitPanel`.
pub(crate) enum ReviewPanelEvent {
    /// The selected PR changed; the git panel should persist the new identity.
    SerializeNeeded,
}

#[derive(Clone)]
struct RecentReview {
    base_branch: SharedString,
    head_branch: SharedString,
    file_count: usize,
    pull_request: Option<PullRequestInfo>,
}

/// Top-level dock navigation, mirroring `GitPanel`'s tab model (Changes |
/// History). Reviewing a specific PR is a drill-down *within* the Pull Requests
/// tab (tracked by `ReviewPanel::reviewing`), not a separate tab.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReviewPanelTab {
    PullRequests,
    Configuration,
}

enum PendingAction {
    OpenDiff(RepoPath),
    SelectPullRequest(PullRequestInfo),
    /// Open the selected PR's combined diff at the top of the first file. Only
    /// queued once both the tree diff and comments have loaded, so the diff
    /// reveals with its inline comments already present.
    OpenPrDiff,
}

pub struct ReviewPanel {
    _workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    active_repository: Option<Entity<Repository>>,
    base_branch: Option<SharedString>,
    head_branch: Option<SharedString>,
    tree_diff: Option<TreeDiff>,
    review_view: Option<(Entity<ReviewView>, Subscription)>,
    focus_handle: FocusHandle,
    recent_reviews_menu_handle: PopoverMenuHandle<ContextMenu>,
    options_menu_handle: PopoverMenuHandle<ContextMenu>,
    active_tab: ReviewPanelTab,
    /// Whether the Pull Requests tab is drilled into a specific PR's review
    /// (the file/thread view) rather than showing the list.
    reviewing: bool,
    recent_reviews: Vec<RecentReview>,
    http_client: Arc<dyn HttpClient>,
    pull_request_list: Option<(Entity<PullRequestList>, Subscription)>,
    configuration_view: Option<(Entity<ConfigurationView>, Subscription)>,
    provider: Option<Arc<dyn ReviewProvider>>,
    remote_owner: Option<String>,
    remote_repo: Option<String>,
    selected_pr: Option<PullRequestInfo>,
    pr_ref_fetch_task: Option<gpui::Task<Result<()>>>,
    pending_action: Option<PendingAction>,
    /// Whether the PR's diff has been auto-opened for the current selection.
    pr_diff_opened: bool,
    /// Handle to the open PR diff, used to release its loading gate once the
    /// diff is fully loaded and comments are injected.
    pr_diff: Option<WeakEntity<crate::project_diff::ProjectDiff>>,
    /// Observes the PR diff so we re-check the reveal gate when it finishes
    /// loading (its final load completion isn't an editor event we'd otherwise see).
    pr_diff_subscription: Option<Subscription>,
    /// Whether the PR diff's loading gate has been released for this selection.
    diff_revealed: bool,
    injected_comment_blocks: HashMap<EntityId, (WeakEntity<Editor>, Vec<CustomBlockId>)>,
    /// Per diff-editor subscriptions used to re-inject inline comment blocks as
    /// the merge diff loads its files incrementally (each file emits
    /// `BufferRangesUpdated` as its excerpts are registered).
    diff_editor_subscriptions: HashMap<EntityId, Subscription>,
    /// Open inline composers (replies and new comments). Multiple can be open at
    /// once; each is identified by `id`.
    inline_composers: Vec<InlineComposer>,
    next_composer_id: usize,
    _workspace_subscription: Option<Subscription>,
    /// Restored-but-not-yet-applied review state (applied once the provider and
    /// remote resolve).
    restore: Option<SerializedReviewPanel>,
}

/// State for an open inline composer (reply to a thread, or a new comment on a
/// line). Both are rendered as blocks produced by the single inject pass, so no
/// separate `insert_blocks` call is made (which previously panicked block_map).
struct InlineComposer {
    id: usize,
    editor: WeakEntity<Editor>,
    input: Entity<Editor>,
    submitting: bool,
    target: ComposerTarget,
}

enum ComposerTarget {
    /// Reply rendered inside the thread rooted at this comment id.
    Reply { in_reply_to: u64 },
    /// A new comment on `line` (1-based) of `path`, anchored to `commit_id`.
    New {
        path: SharedString,
        commit_id: String,
        start_line: Option<u32>,
        line: u32,
    },
}

/// Editor addon that adds a "viewed" checkbox to each file's buffer header in
/// the PR diff, toggling the same per-file viewed state as the Files panel.
struct ReviewEditorAddon {
    review_view: WeakEntity<ReviewView>,
    editor: WeakEntity<Editor>,
}

impl Addon for ReviewEditorAddon {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }

    fn render_buffer_header_controls(
        &self,
        _excerpt: &ExcerptBoundaryInfo,
        buffer: &language::BufferSnapshot,
        _window: &Window,
        cx: &App,
    ) -> Option<AnyElement> {
        let file = buffer.file()?;
        let path =
            SharedString::from(file.path().as_std_path().to_string_lossy().to_string());
        let review_view = self.review_view.upgrade()?;
        let is_viewed = review_view.read(cx).is_file_viewed(&path);
        let review_view = self.review_view.clone();
        Some(
            Checkbox::new(
                SharedString::from(format!("review-viewed-{path}")),
                if is_viewed {
                    ToggleState::Selected
                } else {
                    ToggleState::Unselected
                },
            )
            .on_click(move |_state, _window, cx| {
                let path = path.clone();
                review_view
                    .update(cx, |this, cx| this.toggle_file_viewed(path, cx))
                    .ok();
            })
            .into_any_element(),
        )
    }

    fn extend_mouse_context_menu(
        &self,
        menu: ContextMenu,
        _window: &mut Window,
        _cx: &mut App,
    ) -> ContextMenu {
        menu.action("Add Comment", Box::new(AddComment))
            .separator()
    }

    fn render_gutter_hover_button(
        &self,
        position: editor::Anchor,
        row: editor::display_map::DisplayRow,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<AnyElement> {
        // Only on PR diff editors (where the review view is alive).
        self.review_view.upgrade()?;
        let editor = self.editor.clone();
        Some(
            IconButton::new(("review-add-comment", row.0 as usize), IconName::Plus)
                .icon_size(IconSize::XSmall)
                .style(ui::ButtonStyle::Transparent)
                .tooltip(Tooltip::text("Add comment on this line"))
                .on_click(move |_event, window, cx| {
                    let Some(editor) = editor.upgrade() else {
                        return;
                    };
                    // Move the caret to the hovered line so the AddComment
                    // handler (which reads the selection) targets it.
                    editor.update(cx, |editor, cx| {
                        editor.change_selections(
                            editor::SelectionEffects::no_scroll(),
                            window,
                            cx,
                            |selections| selections.select_anchor_ranges([position..position]),
                        );
                    });
                    window.dispatch_action(Box::new(AddComment), cx);
                })
                .into_any_element(),
        )
    }
}

/// The review view now lives inside the `GitPanel`'s Pull Requests tab rather
/// than a standalone panel; reach it through the git panel.
fn review_panel(workspace: &Workspace, cx: &App) -> Option<Entity<ReviewPanel>> {
    workspace
        .panel::<crate::git_panel::GitPanel>(cx)?
        .read(cx)
        .pull_request_review()
}

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, action: &ApplySuggestion, _window, cx| {
        let comment_id = action.comment_id;
        let Some(panel) = review_panel(workspace, cx) else {
            return;
        };

        let active_editor = workspace
            .active_item(cx)
            .and_then(|item| item.act_as::<Editor>(cx));

        panel.update(cx, |panel, cx| {
            panel.handle_apply_suggestion(comment_id, active_editor, cx);
        });
    });

    workspace.register_action(|workspace, _: &AddComment, window, cx| {
        let Some(panel) = review_panel(workspace, cx) else {
            return;
        };
        let Some(editor) = workspace
            .active_item(cx)
            .and_then(|item| item.act_as::<Editor>(cx))
        else {
            return;
        };
        panel.update(cx, |panel, cx| {
            panel.begin_inline_comment(editor.downgrade(), window, cx);
        });
    });

    workspace.register_action(|workspace, action: &ReactToComment, _window, cx| {
        let Some(panel) = review_panel(workspace, cx) else {
            return;
        };
        let action = action.clone();
        panel.update(cx, |panel, cx| {
            panel.handle_react_to_comment(action, cx);
        });
    });
}

impl ReviewPanel {
    pub(crate) fn new(
        workspace: &Workspace,
        weak_workspace: WeakEntity<Workspace>,
        restore: Option<SerializedReviewPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let project = workspace.project().clone();
        let active_repository = project.read(cx).active_repository(cx);
        let git_store = project.read(cx).git_store().clone();
        cx.subscribe_in(
            &git_store,
            window,
            |this, _store, event, _window, cx| match event {
                GitStoreEvent::ActiveRepositoryChanged(_) => {
                    this.active_repository = this.project.read(cx).active_repository(cx);
                    this.initialize_provider(cx);
                    this.load_branches(cx);
                    cx.notify();
                }
                GitStoreEvent::RepositoryUpdated(_, RepositoryEvent::HeadChanged, _) => {
                    this.load_branches(cx);
                }
                GitStoreEvent::RepositoryUpdated(..) => {
                    if this.provider.is_none() {
                        this.initialize_provider(cx);
                    }
                }
                _ => {}
            },
        )
        .detach();

        let workspace_entity = weak_workspace.upgrade();
        let workspace_subscription = workspace_entity.map(|ws| {
            cx.subscribe(&ws, |this: &mut Self, workspace, event, cx| {
                if let workspace::Event::ActiveItemChanged = event {
                    this.on_active_item_changed(&workspace, cx);
                }
            })
        });

        let mut this = Self {
            _workspace: weak_workspace,
            project,
            active_repository,
            base_branch: None,
            head_branch: None,
            tree_diff: None,
            review_view: None,
            focus_handle: cx.focus_handle(),
            recent_reviews_menu_handle: PopoverMenuHandle::default(),
            options_menu_handle: PopoverMenuHandle::default(),
            active_tab: ReviewPanelTab::PullRequests,
            reviewing: false,
            recent_reviews: Vec::new(),
            http_client: workspace.client().http_client(),
            pull_request_list: None,
            configuration_view: None,
            provider: None,
            remote_owner: None,
            remote_repo: None,
            selected_pr: None,
            pr_ref_fetch_task: None,
            pending_action: None,
            pr_diff_opened: false,
            pr_diff: None,
            pr_diff_subscription: None,
            diff_revealed: false,
            injected_comment_blocks: HashMap::default(),
            diff_editor_subscriptions: HashMap::default(),
            inline_composers: Vec::new(),
            next_composer_id: 0,
            _workspace_subscription: workspace_subscription,
            restore,
        };
        // Create the PR list up front so pull requests start loading in the
        // background as soon as the provider resolves, before the view is opened.
        this.ensure_pull_request_list(window, cx);
        this.initialize_provider(cx);
        this.load_branches(cx);
        this
    }

    /// The current review identity, persisted by the hosting `GitPanel` so
    /// reopening the workspace resumes the same PR.
    pub(crate) fn serialized_state(&self) -> SerializedReviewPanel {
        SerializedReviewPanel {
            remote_owner: self.remote_owner.clone(),
            remote_repo: self.remote_repo.clone(),
            selected_pr_number: self.selected_pr.as_ref().map(|pr| pr.number),
        }
    }

    /// Ask the hosting `GitPanel` to persist the current review identity.
    fn serialize(&mut self, cx: &mut Context<Self>) {
        cx.emit(ReviewPanelEvent::SerializeNeeded);
    }

    /// True when the Pull Requests tab is showing the list (not drilled into a
    /// PR review). Drives whether roving-selection nav is forwarded to the list.
    fn showing_list(&self) -> bool {
        matches!(self.active_tab, ReviewPanelTab::PullRequests) && !self.reviewing
    }

    /// True when the Pull Requests tab is drilled into a specific PR's review.
    fn showing_review(&self) -> bool {
        matches!(self.active_tab, ReviewPanelTab::PullRequests) && self.reviewing
    }

    fn activate_tab(&mut self, tab: ReviewPanelTab, window: &mut Window, cx: &mut Context<Self>) {
        match tab {
            ReviewPanelTab::PullRequests => {
                // Return to the Pull Requests tab, preserving any in-progress
                // drill-down (the `reviewing` flag is untouched).
                self.active_tab = ReviewPanelTab::PullRequests;
                // Reclaim focus so roving keyboard nav is live on the list.
                if !self.reviewing {
                    window.focus(&self.focus_handle, cx);
                }
                cx.notify();
            }
            ReviewPanelTab::Configuration => {
                self.open_configuration(window, cx);
            }
        }
    }


    fn refresh_pull_requests(&mut self, cx: &mut Context<Self>) {
        if let Some((pr_list, _)) = &self.pull_request_list {
            pr_list.update(cx, |list, cx| list.refresh(cx));
        }
    }

    fn render_recent_reviews_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let recent = self.recent_reviews.clone();
        let weak_panel = cx.weak_entity();

        PopoverMenu::new("review-nav-menu")
            .trigger_with_tooltip(
                IconButton::new("review-nav-menu", IconName::Menu)
                    .icon_size(IconSize::Small),
                Tooltip::text("Recent Reviews"),
            )
            .anchor(gpui::Anchor::TopRight)
            .with_handle(self.recent_reviews_menu_handle.clone())
            .menu(move |window, cx| {
                let recent = recent.clone();
                let weak_panel = weak_panel.clone();
                Some(ContextMenu::build(
                    window,
                    cx,
                    move |mut menu, _window, _cx| {
                        if recent.is_empty() {
                            return menu.entry("No recent reviews", None, |_window, _cx| {});
                        }

                        menu = menu.header("Recent");
                        for entry in &recent {
                            let label = if let Some(pr) = &entry.pull_request {
                                format!(
                                    "#{} {}..{} ({} files)",
                                    pr.number,
                                    entry.base_branch,
                                    entry.head_branch,
                                    entry.file_count
                                )
                            } else {
                                format!(
                                    "{}..{} ({} files)",
                                    entry.base_branch,
                                    entry.head_branch,
                                    entry.file_count
                                )
                            };
                            let base = entry.base_branch.clone();
                            let head = entry.head_branch.clone();
                            let pull_request = entry.pull_request.clone();
                            let weak_panel = weak_panel.clone();
                            menu = menu.entry(label, None, move |_window, cx| {
                                weak_panel
                                    .update(cx, |this, cx| {
                                        this.base_branch = Some(base.clone());
                                        this.head_branch = Some(head.clone());
                                        if let Some(pr) = &pull_request {
                                            this.select_pull_request(pr, cx);
                                        } else {
                                            this.selected_pr = None;
                                            this.review_view = None;
                                            this.load_diff(cx);
                                        }
                                    })
                                    .ok();
                            });
                        }
                        menu
                    },
                ))
            })
    }

    fn render_options_menu(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let weak_panel = cx.weak_entity();
        let show_tree_toggle = self.showing_review();
        let is_tree_view = if self.showing_review() {
            self.review_view
                .as_ref()
                .map(|(rv, _)| rv.read(cx).is_tree_view())
                .unwrap_or(false)
        } else {
            false
        };
        let is_review_thread = self.showing_review();

        PopoverMenu::new("review-options-menu")
            .trigger_with_tooltip(
                IconButton::new("review-options-menu", IconName::Ellipsis)
                    .icon_size(IconSize::Small),
                Tooltip::text("Options"),
            )
            .anchor(gpui::Anchor::TopRight)
            .with_handle(self.options_menu_handle.clone())
            .menu(move |window, cx| {
                let weak_panel = weak_panel.clone();
                Some(ContextMenu::build(window, cx, move |menu, _window, _| {
                    menu.when(show_tree_toggle, |menu| {
                        let weak_panel = weak_panel.clone();
                        menu.entry(
                            if is_tree_view {
                                "Flat View"
                            } else {
                                "Tree View"
                            },
                            None,
                            {
                                move |_window, cx| {
                                    weak_panel
                                        .update(cx, |this, cx| {
                                            if is_review_thread
                                                && let Some((review_view, _)) = &this.review_view
                                            {
                                                review_view.update(cx, |rv, cx| {
                                                    rv.toggle_tree_view(cx);
                                                });
                                            }
                                        })
                                        .ok();
                                }
                            },
                        )
                        .separator()
                    })
                    .entry("Configuration", None, {
                        let weak_panel = weak_panel.clone();
                        move |window, cx| {
                            weak_panel
                                .update(cx, |this, cx| this.open_configuration(window, cx))
                                .ok();
                        }
                    })
                    .separator()
                    .entry("Full Screen", None, |_window, _cx| {
                        // TODO: dispatch ToggleZoom action
                    })
                    .separator()
                    .entry("Refresh", None, {
                        move |_window, cx| {
                            weak_panel
                                .update(cx, |this, cx| {
                                    this.refresh_pull_requests(cx);
                                })
                                .ok();
                        }
                    })
                }))
            })
    }

    /// The single functional header bar for the current view. The standalone
    /// "Review" name bar is gone — panel identity comes from the dock tab, and
    /// each view folds its controls and actions into this one row (matching the
    /// git panel / Agent settings). Returns `None` for the Configuration view,
    /// which renders its own `← GitHub account` bar.
    fn render_header(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let bar = h_flex()
            .id("review-panel-header")
            .h(Tab::container_height(cx))
            .max_w_full()
            .flex_none()
            .gap(DynamicSpacing::Base04.rems(cx))
            .px(DynamicSpacing::Base04.rems(cx))
            .bg(cx.theme().colors().tab_bar_background)
            .border_b_1()
            .border_color(cx.theme().colors().border);

        let actions = h_flex()
            .flex_none()
            .gap(DynamicSpacing::Base02.rems(cx))
            .child(self.render_recent_reviews_menu(cx))
            .child(self.render_options_menu(window, cx));

        if self.showing_review() {
            let title = self.selected_pr.as_ref().map(|pr| {
                (SharedString::from(format!("#{}", pr.number)), pr.title.clone())
            });
            return Some(
                bar.child(
                    IconButton::new("back-to-pr-list", IconName::ArrowLeft)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Back to pull requests"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.close_review(window, cx);
                        })),
                )
                .child(
                    h_flex()
                        .flex_1()
                        .gap_1()
                        .overflow_x_hidden()
                        .items_center()
                        .when_some(title, |row, (number, pr_title)| {
                            row.child(Label::new(number).color(Color::Muted)).child(
                                div()
                                    .overflow_x_hidden()
                                    .flex_1()
                                    .child(Label::new(pr_title).single_line()),
                            )
                        }),
                )
                .child(actions)
                .into_any_element(),
            );
        }

        if self.showing_list() {
            let filter_bar = self
                .pull_request_list
                .as_ref()
                .map(|(list, _)| list.update(cx, |list, cx| list.render_filter_bar(cx)));
            return Some(
                bar.child(div().flex_1().children(filter_bar))
                    .child(
                        actions
                            .child(
                                IconButton::new("new-review", IconName::Plus)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("New Review"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.show_pull_request_list(window, cx);
                                    })),
                            )
                            .child(
                                IconButton::new("configure-review", IconName::Settings)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Configuration"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.activate_tab(
                                            ReviewPanelTab::Configuration,
                                            window,
                                            cx,
                                        );
                                    })),
                            ),
                    )
                    .into_any_element(),
            );
        }

        // Configuration view: no tab bar, so surface a back arrow and title.
        Some(
            bar.child(
                IconButton::new("back-from-config", IconName::ArrowLeft)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Back to pull requests"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.activate_tab(ReviewPanelTab::PullRequests, window, cx);
                    })),
            )
            .child(
                h_flex()
                    .flex_1()
                    .child(Label::new("Configuration").color(Color::Muted)),
            )
            .into_any_element(),
        )
    }

    fn set_remote_state(&mut self, state: RemoteState, cx: &mut Context<Self>) {
        if let Some((pr_list, _)) = &self.pull_request_list {
            pr_list.update(cx, |list, cx| list.set_remote_state(state, cx));
        }
    }

    fn initialize_provider(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.active_repository.clone() else {
            // The active repository hasn't been resolved yet (git is still
            // loading); keep showing the loader rather than flashing the
            // "no remote" empty state. A later repository event re-runs this.
            log::info!("review_panel: no active repository");
            self.set_remote_state(RemoteState::Resolving, cx);
            return;
        };

        let remote_url = repo.read(cx).default_remote_url();
        log::info!("review_panel: remote_url = {:?}", remote_url);
        let Some(remote_url) = remote_url else {
            // The repository's remotes may not be populated yet right after it
            // becomes active; keep the loader rather than flashing the empty
            // state. A later repository event re-runs this once remotes load.
            log::info!("review_panel: no remote URL found");
            self.set_remote_state(RemoteState::Resolving, cx);
            return;
        };

        let Ok((owner, repo_name)) = parse_github_remote(&remote_url) else {
            // We have a remote URL and it definitively isn't GitHub.
            log::info!("review_panel: failed to parse remote URL: {}", remote_url);
            self.set_remote_state(RemoteState::Unavailable, cx);
            return;
        };
        log::info!("review_panel: parsed {}/{}", owner, repo_name);

        let http_client = self.http_client.clone();
        let credentials_provider = zed_credentials_provider::global(cx);

        self.remote_owner = Some(owner);
        self.remote_repo = Some(repo_name);
        self.set_remote_state(RemoteState::Resolving, cx);

        cx.spawn(async move |this, cx| {
            let token = resolve_github_token(credentials_provider, cx).await;

            let provider: Arc<dyn ReviewProvider> =
                Arc::new(GitHubProvider::new(http_client, token));

            this.update(cx, |this, cx| {
                this.provider = Some(provider.clone());
                if let (Some((pr_list, _)), Some(owner), Some(repo)) =
                    (&this.pull_request_list, this.remote_owner.clone(), this.remote_repo.clone())
                {
                    pr_list.update(cx, |list, cx| {
                        list.set_provider(provider, owner, repo, cx);
                    });
                }
                this.maybe_restore_selected_pr(cx);
                cx.notify();
            })?;

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn select_pull_request(&mut self, pr: &PullRequestInfo, cx: &mut Context<Self>) {
        self.remove_all_injected_blocks(cx);
        self.selected_pr = Some(pr.clone());
        self.base_branch = Some(pr.base_ref.clone());
        self.head_branch = Some(pr.head_ref.clone());
        self.pr_diff_opened = false;
        self.pr_diff = None;
        self.pr_diff_subscription = None;
        self.diff_revealed = false;

        self.fetch_pr_ref(pr.number, cx);
        self.pending_action = Some(PendingAction::SelectPullRequest(pr.clone()));
        self.active_tab = ReviewPanelTab::PullRequests;
        self.reviewing = true;
        cx.notify();
        self.serialize(cx);
    }

    /// Restore the previously-reviewed PR once the provider and remote resolve.
    /// The PR is re-fetched so its head SHA is current.
    fn maybe_restore_selected_pr(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.restore.take() else {
            return;
        };
        let (Some(number), Some(provider), Some(owner), Some(repo)) = (
            state.selected_pr_number,
            self.provider.clone(),
            self.remote_owner.clone(),
            self.remote_repo.clone(),
        ) else {
            return;
        };
        // Only restore into the same repository the state was saved for.
        if state.remote_owner.as_deref() != Some(owner.as_str())
            || state.remote_repo.as_deref() != Some(repo.as_str())
            || self.selected_pr.is_some()
        {
            return;
        }
        cx.spawn(async move |this, cx| {
            let details = provider
                .fetch_pull_request_details(&owner, &repo, number)
                .await?;
            this.update(cx, |this, cx| {
                this.select_pull_request(&details.info, cx);
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn create_review_view(
        &mut self,
        pr: &PullRequestInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let review_view = cx.new(|cx| {
            ReviewView::new(
                self.provider.clone(),
                self.remote_owner.clone(),
                self.remote_repo.clone(),
                pr.clone(),
                window,
                cx,
            )
        });
        let subscription =
            cx.subscribe_in(&review_view, window, |this, _view, event, window, cx| {
                match event {
                    ReviewViewEvent::OpenFileDiff(path) => {
                        this.open_file_diff(path.clone(), cx);
                    }
                    ReviewViewEvent::NavigateToComment { path, line } => {
                        this.navigate_to_comment(path.clone(), *line, window, cx);
                    }
                    ReviewViewEvent::SetFileDiffFolded { path, folded } => {
                        this.set_file_diff_folded(path.clone(), *folded, cx);
                    }
                    ReviewViewEvent::CommentsChanged => {
                        // Comments just settled: open the diff if the tree is
                        // ready. While held, only check the reveal gate (which
                        // injects once at reveal); once revealed, reinject live.
                        this.maybe_open_pr_diff(cx);
                        if this.diff_revealed {
                            this.refresh_inline_comment_blocks(cx);
                        } else {
                            this.maybe_reveal_diff(cx);
                        }
                    }
                    ReviewViewEvent::Merged => {
                        this.refresh_pull_requests(cx);
                    }
                }
            });
        self.review_view = Some((review_view, subscription));
    }

    fn fetch_pr_ref(&mut self, pr_number: u32, cx: &mut Context<Self>) {
        let Some(repo) = self.active_repository.clone() else {
            return;
        };

        let work_dir = repo.read(cx).snapshot().work_directory_abs_path;
        let refspec = format!("pull/{}/head", pr_number);
        // Also fetch the base branch so the PR's base SHA (and the merge-base
        // with the head) is present locally for the diff.
        let base_ref = self
            .selected_pr
            .as_ref()
            .map(|pr| pr.base_ref.to_string());

        self.pr_ref_fetch_task = Some(cx.spawn(async move |this, cx| {
            let mut args = vec!["fetch".to_string(), "origin".to_string(), refspec.clone()];
            if let Some(base_ref) = &base_ref {
                args.push(base_ref.clone());
            }
            let output = smol::process::Command::new("git")
                .current_dir(work_dir.as_ref())
                .args(&args)
                .output()
                .await?;

            if output.status.success() {
                log::info!("review_panel: fetched PR #{} ref", pr_number);
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                log::warn!(
                    "review_panel: PR #{} ref fetch failed: {}",
                    pr_number,
                    stderr
                );
            }

            this.update(cx, |this, cx| {
                this.load_diff(cx);
            })?;
            anyhow::Ok(())
        }));
    }

    fn load_branches(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.active_repository.clone() else {
            return;
        };

        let default_branch_rx = repo.update(cx, |repo, _cx| repo.default_branch(false));
        let branches_rx = repo.update(cx, |repo, _cx| repo.branches());

        cx.spawn(async move |this, cx| {
            if let Ok(Some(default)) = default_branch_rx.await? {
                this.update(cx, |this, cx| {
                    this.base_branch = Some(default);
                    cx.notify();
                })?;
            }

            if let Ok(branches) = branches_rx.await? {
                let head = branches
                    .branches
                    .iter()
                    .find(|b| b.is_head)
                    .map(|b| b.ref_name.clone());
                if let Some(head) = head {
                    this.update(cx, |this, cx| {
                        // Only record the branch names as fallback state; don't
                        // auto-run the local branch diff (the panel is PR-focused).
                        this.head_branch = Some(head);
                        cx.notify();
                    })?;
                }
            }

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn load_diff(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.active_repository.clone() else {
            return;
        };

        // For a PR, diff the recorded commit SHAs (matches GitHub's three-dot
        // "Files changed"). Falling back to branch names would resolve the base
        // to the local — often stale — branch tip and show a huge bogus diff.
        let find_renames = self.selected_pr.is_some();
        let (base, head) = if let Some(pr) = &self.selected_pr {
            (pr.base_sha.clone(), pr.head_sha.clone())
        } else {
            let (Some(base), Some(head)) = (self.base_branch.clone(), self.head_branch.clone())
            else {
                return;
            };
            (base, head)
        };

        let diff_rx = repo.update(cx, |repo, cx| {
            repo.diff_tree(
                DiffTreeType::MergeBase {
                    base,
                    head,
                    find_renames,
                },
                cx,
            )
        });

        cx.spawn(async move |this, cx| {
            let tree_diff = diff_rx.await??;
            this.update(cx, |this, cx| {
                this.tree_diff = Some(tree_diff);

                let file_count = this
                    .tree_diff
                    .as_ref()
                    .map(|d| d.entries.len())
                    .unwrap_or(0);

                if let (Some(base), Some(head)) =
                    (this.base_branch.clone(), this.head_branch.clone())
                {
                    let pull_request = this.selected_pr.clone();
                    this.recent_reviews
                        .retain(|r| !(r.base_branch == base && r.head_branch == head));
                    this.recent_reviews.insert(
                        0,
                        RecentReview {
                            base_branch: base,
                            head_branch: head,
                            file_count,
                            pull_request,
                        },
                    );
                    this.recent_reviews.truncate(10);
                }

                if let Some((review_view, _)) = &this.review_view {
                    review_view.update(cx, |view, cx| {
                        view.set_tree_diff(this.tree_diff.as_ref(), cx);
                    });
                }

                // The tree diff is now ready; open the combined diff if comments
                // have also finished loading (otherwise the CommentsChanged event
                // will trigger it). Opens at the top of the first file with its
                // comments already inline.
                this.maybe_open_pr_diff(cx);
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }


    fn ensure_pull_request_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pull_request_list.is_some() {
            return;
        }
        let pr_list = cx.new(|cx| {
            PullRequestList::new(
                self.provider.clone(),
                self.remote_owner.clone(),
                self.remote_repo.clone(),
                window,
                cx,
            )
        });
        let subscription = cx.subscribe(&pr_list, |this, _pr_list, event, cx| match event {
            PullRequestListEvent::Selected(pr) => {
                this.select_pull_request(&pr.clone(), cx);
            }
        });
        self.pull_request_list = Some((pr_list, subscription));
    }

    fn show_pull_request_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ensure_pull_request_list(window, cx);

        if let Some((pr_list, _)) = &self.pull_request_list {
            pr_list.update(cx, |list, cx| list.load_if_empty(cx));
        }

        self.active_tab = ReviewPanelTab::PullRequests;
        self.reviewing = false;
        // Reviewing a PR moves focus to the center-pane diff; returning to the
        // list must reclaim focus so the `ReviewPanel` key context is active and
        // roving keyboard navigation works again.
        window.focus(&self.focus_handle, cx);
        self.serialize(cx);
        cx.notify();
    }

    /// Leave the current PR review entirely: tear down the review view, injected
    /// comment blocks, and diff subscriptions, then return to the list.
    fn close_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_pr = None;
        self.review_view = None;
        self.remove_all_injected_blocks(cx);
        self.diff_editor_subscriptions.clear();
        self.pr_diff = None;
        self.pr_diff_subscription = None;
        self.diff_revealed = false;
        self.pr_diff_opened = false;
        self.show_pull_request_list(window, cx);
    }

    fn open_configuration(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let credentials = zed_credentials_provider::global(cx);
        let view = cx.new(|cx| {
            ConfigurationView::new(
                credentials,
                self.http_client.clone(),
                self.remote_owner.clone(),
                self.remote_repo.clone(),
                window,
                cx,
            )
        });
        let subscription = cx.subscribe(&view, |this, _view, event, cx| match event {
            ConfigurationEvent::CredentialsChanged => {
                // Rebuild the provider with the new token (set_provider reloads
                // the list) and return to the pull request list.
                this.initialize_provider(cx);
                this.active_tab = ReviewPanelTab::PullRequests;
                this.reviewing = false;
                cx.notify();
            }
            ConfigurationEvent::Back => {
                this.active_tab = ReviewPanelTab::PullRequests;
                this.reviewing = false;
                cx.notify();
            }
        });
        self.configuration_view = Some((view, subscription));
        self.active_tab = ReviewPanelTab::Configuration;
        cx.notify();
    }

    fn open_file_diff(&mut self, path: RepoPath, cx: &mut Context<Self>) {
        self.pending_action = Some(PendingAction::OpenDiff(path));
        cx.notify();
    }

    /// Open the selected PR's combined diff once the tree diff is ready (the PR
    /// ref is available locally). The diff opens at the top of the first file,
    /// held behind a loading state; `maybe_reveal_diff` releases it once files
    /// are loaded and comments injected. Queues a pending action so the deploy
    /// (which needs a Window) happens in the render flush.
    fn maybe_open_pr_diff(&mut self, cx: &mut Context<Self>) {
        if self.pr_diff_opened || self.selected_pr.is_none() || self.tree_diff.is_none() {
            return;
        }
        self.pr_diff_opened = true;
        self.pending_action = Some(PendingAction::OpenPrDiff);
        cx.notify();
    }

    /// Reveal the held PR diff once the comments have loaded (and been injected)
    /// and the diff has finished loading all its files, so the diff and its
    /// inline comments appear together instead of streaming in.
    fn maybe_reveal_diff(&mut self, cx: &mut Context<Self>) {
        if self.diff_revealed {
            return;
        }
        let Some(diff) = self.pr_diff.as_ref().and_then(|diff| diff.upgrade()) else {
            return;
        };
        let comments_ready = self
            .review_view
            .as_ref()
            .is_some_and(|(view, _)| view.read(cx).comments_ready());
        if !comments_ready || !diff.read(cx).is_fully_loaded(cx) {
            return;
        }
        self.diff_revealed = true;
        // Inject all comment blocks synchronously *before* releasing the gate, so
        // the diff reveals with its comments already in place (no drop-in). The
        // incremental injections during the held phase happen off-screen.
        if let Some(editor) = self.pr_diff_editor(cx) {
            self.reinject_inline_blocks(&editor, cx);
        }
        diff.update(cx, |diff, cx| diff.set_review_loading(false, cx));
    }

    /// Fold or unfold a file's diff in the combined diff editor (no-op if it's
    /// already in the requested state or the file isn't present yet). Mirrors
    /// GitHub collapsing a diff once it's marked viewed and re-expanding it when
    /// unviewed.
    fn set_file_diff_folded(&mut self, path: RepoPath, folded: bool, cx: &mut Context<Self>) {
        let Some(editor) = self.pr_diff_editor(cx) else {
            return;
        };
        let target_path = path.as_std_path().to_string_lossy().to_string();
        editor.update(cx, |editor, cx| {
            let buffer_id = editor
                .buffer()
                .read(cx)
                .all_buffers()
                .into_iter()
                .find_map(|buffer| {
                    let buffer = buffer.read(cx);
                    let file = buffer.file()?;
                    let file_path = file.path().as_std_path().to_string_lossy().to_string();
                    (file_path == target_path).then(|| buffer.remote_id())
                });
            let Some(buffer_id) = buffer_id else {
                return;
            };
            if folded && !editor.is_buffer_folded(buffer_id, cx) {
                editor.fold_buffers(vec![buffer_id], cx);
            } else if !folded && editor.is_buffer_folded(buffer_id, cx) {
                editor.unfold_buffer(buffer_id, cx);
            }
        });
    }

    /// The active (or last-injected) PR diff editor, if one is open.
    fn pr_diff_editor(&self, cx: &App) -> Option<Entity<Editor>> {
        self._workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).active_item(cx))
            .and_then(|item| item.act_as::<Editor>(cx))
            .filter(|editor| editor.read(cx).addon::<ReviewEditorAddon>().is_some())
            .or_else(|| {
                self.injected_comment_blocks
                    .values()
                    .find_map(|(editor, _)| editor.upgrade())
            })
    }

    /// Resolve a (path, line) to a multibuffer anchor in `editor` and center the
    /// view on it. Returns `false` when the file's excerpt isn't loaded yet (so
    /// the caller can retry once the diff finishes loading that file).
    fn scroll_diff_editor_to_comment(
        editor: &Entity<Editor>,
        path: &RepoPath,
        line: u32,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        let target_path = path.as_std_path().to_string_lossy().to_string();
        let row = line.saturating_sub(1);
        let focus_handle = editor.read(cx).focus_handle(cx);
        let scrolled = editor.update(cx, |editor, cx| {
            let multibuffer = editor.buffer().clone();
            let snapshot = multibuffer.read(cx).snapshot(cx);
            let anchor = multibuffer
                .read(cx)
                .all_buffers()
                .into_iter()
                .find_map(|buffer| {
                    let buffer = buffer.read(cx);
                    let file = buffer.file()?;
                    let file_path = file.path().as_std_path().to_string_lossy().to_string();
                    if file_path != target_path {
                        return None;
                    }
                    let buffer_snapshot = buffer.snapshot();
                    if row > buffer_snapshot.max_point().row {
                        return None;
                    }
                    snapshot.anchor_in_excerpt(buffer_snapshot.anchor_before(Point::new(row, 0)))
                });
            let Some(anchor) = anchor else {
                return false;
            };
            editor.change_selections(
                editor::SelectionEffects::scroll(editor::scroll::Autoscroll::center()),
                window,
                cx,
                |selections| selections.select_anchor_ranges([anchor..anchor]),
            );
            true
        });
        if scrolled {
            window.focus(&focus_handle, cx);
        }
        scrolled
    }

    /// Scroll the combined PR diff to a line-anchored comment so the reviewer
    /// sees the thread inline. The merge diff loads its files asynchronously, so
    /// if the target file's excerpt isn't present yet we nudge the diff open and
    /// retry as it loads.
    fn navigate_to_comment(
        &mut self,
        path: RepoPath,
        line: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self.pr_diff_editor(cx) else {
            self.open_file_diff(path, cx);
            return;
        };

        if Self::scroll_diff_editor_to_comment(&editor, &path, line, window, cx) {
            return;
        }

        // The file's excerpt hasn't loaded yet — open it and retry as the diff
        // populates (each file registers its excerpts incrementally).
        self.open_file_diff(path.clone(), cx);
        cx.spawn_in(window, async move |this, cx| {
            for _ in 0..40 {
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
                let done = this.update_in(cx, |this, window, cx| {
                    let Some(editor) = this.pr_diff_editor(cx) else {
                        return false;
                    };
                    Self::scroll_diff_editor_to_comment(&editor, &path, line, window, cx)
                })?;
                if done {
                    break;
                }
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn on_active_item_changed(&mut self, workspace: &Entity<Workspace>, cx: &mut Context<Self>) {
        if self.selected_pr.is_none() {
            return;
        }
        let Some((review_view, _)) = &self.review_view else {
            return;
        };

        let active_item = workspace.read(cx).active_item(cx);
        let Some(item) = active_item else {
            return;
        };
        let Some(editor) = item.act_as::<Editor>(cx) else {
            return;
        };

        // Capture the PR diff entity so we can release its loading gate once the
        // diff is fully loaded and comments are injected. Observe it so the
        // load-completion notify re-checks the reveal gate.
        if self.pr_diff.is_none() {
            if let Some(diff) = item.downcast::<crate::project_diff::ProjectDiff>() {
                self.pr_diff = Some(diff.downgrade());
                self.pr_diff_subscription = Some(cx.observe(&diff, |this, _diff, cx| {
                    this.maybe_reveal_diff(cx);
                }));
            }
        }

        // Add the per-file "viewed" checkbox to the diff editor's buffer headers.
        // (The "Add Comment" right-click entry is contributed by the addon's
        // extend_mouse_context_menu hook.)
        if editor.read(cx).addon::<ReviewEditorAddon>().is_none() {
            let review_view = review_view.downgrade();
            let weak_editor = editor.downgrade();
            editor.update(cx, |editor, _cx| {
                editor.register_addon(ReviewEditorAddon {
                    review_view,
                    editor: weak_editor,
                });
            });
        }

        let editor_id = editor.entity_id();

        // The merge diff registers each file's excerpts asynchronously as buffers
        // load, emitting `BufferRangesUpdated`. Re-inject (re-resolving every
        // anchor against the now-larger multibuffer) so comments land on their
        // real lines regardless of load order.
        if let std::collections::hash_map::Entry::Vacant(entry) =
            self.diff_editor_subscriptions.entry(editor_id)
        {
            let subscription =
                cx.subscribe(&editor, |this, _editor, event: &EditorEvent, cx| {
                    if let EditorEvent::BufferRangesUpdated { .. } = event {
                        // Only used to drive the reveal check while the diff is
                        // held. We deliberately do NOT re-inject here: comments
                        // are injected once at reveal, and their anchors stay
                        // valid across later excerpt changes — re-injecting would
                        // churn block heights and visibly reflow the diff.
                        this.maybe_reveal_diff(cx);
                    }
                });
            entry.insert(subscription);
        }

        if self.injected_comment_blocks.contains_key(&editor_id) {
            return;
        }

        let singleton_buffer = editor.read(cx).buffer().read(cx).as_singleton();
        if let Some(buffer) = singleton_buffer {
            let file_path = buffer.read(cx).file().map(|file| {
                SharedString::from(file.path().as_std_path().to_string_lossy().to_string())
            });
            if let Some(file_path) = file_path {
                let comments = review_view.read(cx).comments_for_file(&file_path);
                if !comments.is_empty() {
                    self.inject_pr_comments_into_editor(&editor, &comments, cx);
                }
            }
            return;
        }

        self.inject_for_multibuffer_editor(&editor, &review_view.clone(), cx);
    }

    fn inject_for_multibuffer_editor(
        &mut self,
        editor: &Entity<Editor>,
        review_view: &Entity<ReviewView>,
        cx: &mut Context<Self>,
    ) {
        let multibuffer = editor.read(cx).buffer().clone();
        let snapshot = multibuffer.read(cx).snapshot(cx);
        let buffers = multibuffer.read(cx).all_buffers();

        let weak_panel = cx.weak_entity();
        let weak_editor = editor.downgrade();
        let mut blocks = Vec::new();
        for buffer in buffers {
            let (file_path, buffer_snapshot) = {
                let buffer = buffer.read(cx);
                let Some(file) = buffer.file() else {
                    continue;
                };
                let file_path = SharedString::from(
                    file.path().as_std_path().to_string_lossy().to_string(),
                );
                (file_path, buffer.snapshot())
            };

            let comments = review_view.read(cx).comments_for_file(&file_path);
            if comments.is_empty() {
                continue;
            }
            let threads = Self::build_comment_threads(&comments, cx);
            let max_row = buffer_snapshot.max_point().row;

            for (line, thread_comments) in threads {
                let row = line.saturating_sub(1);
                if row > max_row {
                    continue;
                }
                // Lift the buffer-relative position into a multibuffer anchor. Returns
                // None when the line isn't inside a displayed excerpt (e.g. a comment
                // on an unchanged line not shown in the diff) — skip those.
                let text_anchor = buffer_snapshot.anchor_before(Point::new(row, 0));
                let Some(anchor) = snapshot.anchor_in_excerpt(text_anchor) else {
                    continue;
                };
                let root_id = thread_comments.first().map(|(comment, _, _)| comment.id);
                let composing = root_id.is_some_and(|id| self.is_composing_for(id));
                let height = Self::estimate_block_height(&thread_comments)
                    + 2
                    + if composing { 12 } else { 0 };
                let weak_panel = weak_panel.clone();
                let weak_editor = weak_editor.clone();
                blocks.push(BlockProperties {
                    placement: BlockPlacement::Below(anchor),
                    height: Some(height),
                    style: BlockStyle::Sticky,
                    render: Arc::new(move |cx| {
                        render_comment_thread_with_reply(
                            thread_comments.clone(),
                            root_id,
                            weak_panel.clone(),
                            weak_editor.clone(),
                            cx,
                        )
                    }),
                    priority: 0,
                });
            }
        }

        // Pending new-comment composers are emitted in the same insert pass.
        for composer in &self.inline_composers {
            let ComposerTarget::New { path, line, .. } = &composer.target else {
                continue;
            };
            let row = line.saturating_sub(1);
            let anchor = multibuffer.read(cx).all_buffers().into_iter().find_map(|buffer| {
                let buffer = buffer.read(cx);
                let file = buffer.file()?;
                let file_path =
                    SharedString::from(file.path().as_std_path().to_string_lossy().to_string());
                if &file_path != path {
                    return None;
                }
                let buffer_snapshot = buffer.snapshot();
                if row > buffer_snapshot.max_point().row {
                    return None;
                }
                snapshot.anchor_in_excerpt(buffer_snapshot.anchor_before(Point::new(row, 0)))
            });
            if let Some(anchor) = anchor {
                let input = composer.input.clone();
                let composer_id = composer.id;
                let weak_panel = weak_panel.clone();
                blocks.push(BlockProperties {
                    placement: BlockPlacement::Below(anchor),
                    height: Some(6),
                    style: BlockStyle::Sticky,
                    render: Arc::new(move |cx| {
                        render_composer_block(input.clone(), composer_id, weak_panel.clone(), cx)
                    }),
                    priority: 1,
                });
            }
        }

        if blocks.is_empty() {
            return;
        }

        let editor_id = editor.entity_id();
        let block_ids = editor.update(cx, |editor, cx| editor.insert_blocks(blocks, None, cx));
        self.injected_comment_blocks
            .insert(editor_id, (editor.downgrade(), block_ids));
    }

    fn inject_pr_comments_into_editor(
        &mut self,
        editor: &Entity<Editor>,
        comments: &[ReviewComment],
        cx: &mut Context<Self>,
    ) {
        let threads = Self::build_comment_threads(comments, cx);
        if threads.is_empty() {
            return;
        }

        let editor_id = editor.entity_id();
        let weak_panel = cx.weak_entity();
        let weak_editor = editor.downgrade();
        let composing_roots: Vec<u64> = self
            .inline_composers
            .iter()
            .filter_map(|composer| match &composer.target {
                ComposerTarget::Reply { in_reply_to } => Some(*in_reply_to),
                ComposerTarget::New { .. } => None,
            })
            .collect();
        let block_ids = editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let max_row = snapshot.max_point().row;

            let blocks: Vec<_> = threads
                .into_iter()
                .filter_map(|(line, thread_comments)| {
                    let row = line.saturating_sub(1);
                    if row > max_row {
                        return None;
                    }
                    let anchor = snapshot.anchor_before(Point::new(row, 0));
                    let root_id = thread_comments.first().map(|(comment, _, _)| comment.id);
                    let composing = root_id.is_some_and(|id| composing_roots.contains(&id));
                    let height = Self::estimate_block_height(&thread_comments)
                        + 2
                        + if composing { 12 } else { 0 };
                    let thread_clone = thread_comments;
                    let weak_panel = weak_panel.clone();
                    let weak_editor = weak_editor.clone();
                    Some(BlockProperties {
                        placement: BlockPlacement::Below(anchor),
                        height: Some(height),
                        style: BlockStyle::Sticky,
                        render: Arc::new(move |cx| {
                            render_comment_thread_with_reply(
                                thread_clone.clone(),
                                root_id,
                                weak_panel.clone(),
                                weak_editor.clone(),
                                cx,
                            )
                        }),
                        priority: 0,
                    })
                })
                .collect();

            editor.insert_blocks(blocks, None, cx)
        });

        self.injected_comment_blocks
            .insert(editor_id, (editor.downgrade(), block_ids));
    }

    fn remove_all_injected_blocks(&mut self, cx: &mut Context<Self>) {
        let entries: Vec<_> = self.injected_comment_blocks.drain().collect();
        for (_editor_id, (weak_editor, block_ids)) in entries {
            if let Some(editor) = weak_editor.upgrade() {
                let block_id_set = block_ids.into_iter().collect();
                editor.update(cx, |editor, cx| {
                    editor.remove_blocks(block_id_set, None, cx);
                });
            }
        }
    }

    /// Open the reply composer for the thread rooted at `in_reply_to`. The
    /// composer is rendered inside that thread's existing inline block (no new
    /// block is inserted), so re-inject to pick up the change.
    fn begin_inline_reply(
        &mut self,
        target_editor: WeakEntity<Editor>,
        in_reply_to: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = target_editor.upgrade() else {
            return;
        };
        // Only one reply composer per thread; focus the existing one if open.
        if let Some(existing) = self.inline_composers.iter().find(|c| {
            matches!(&c.target, ComposerTarget::Reply { in_reply_to: id } if *id == in_reply_to)
        }) {
            window.focus(&existing.input.focus_handle(cx), cx);
            cx.notify();
            return;
        }

        let input = cx.new(|cx| {
            let mut editor = Editor::auto_height(2, 4, window, cx);
            editor.set_placeholder_text("Reply…", window, cx);
            editor.set_show_gutter(false, cx);
            editor.set_show_wrap_guides(false, cx);
            editor.set_show_indent_guides(false, cx);
            editor.set_use_autoclose(false);
            editor
        });
        window.focus(&input.focus_handle(cx), cx);

        self.add_composer(target_editor, input, ComposerTarget::Reply { in_reply_to });
        self.reinject_inline_blocks(&editor, cx);
        cx.notify();
    }

    fn add_composer(
        &mut self,
        editor: WeakEntity<Editor>,
        input: Entity<Editor>,
        target: ComposerTarget,
    ) -> usize {
        let id = self.next_composer_id;
        self.next_composer_id += 1;
        self.inline_composers.push(InlineComposer {
            id,
            editor,
            input,
            submitting: false,
            target,
        });
        id
    }

    /// Open a new-comment composer for the active editor's current selection
    /// (single line, or a line range for a multi-line selection).
    fn begin_inline_comment(
        &mut self,
        target_editor: WeakEntity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = target_editor.upgrade() else {
            return;
        };
        let Some(commit_id) = self.selected_pr.as_ref().map(|pr| pr.head_sha.to_string()) else {
            return;
        };
        let Some((path, start_line, line)) = Self::selection_target(&editor, cx) else {
            return;
        };

        let input = cx.new(|cx| {
            let mut editor = Editor::auto_height(2, 6, window, cx);
            editor.set_placeholder_text("Add a comment…", window, cx);
            editor.set_show_gutter(false, cx);
            editor.set_show_wrap_guides(false, cx);
            editor.set_show_indent_guides(false, cx);
            editor.set_use_autoclose(false);
            editor
        });
        window.focus(&input.focus_handle(cx), cx);

        self.add_composer(
            target_editor,
            input,
            ComposerTarget::New {
                path,
                commit_id,
                start_line,
                line,
            },
        );
        self.reinject_inline_blocks(&editor, cx);
        cx.notify();
    }

    /// Resolve the active selection in a multibuffer diff editor to a file path
    /// and 1-based line range (RIGHT side / head content).
    fn selection_target(
        editor: &Entity<Editor>,
        cx: &mut Context<Self>,
    ) -> Option<(SharedString, Option<u32>, u32)> {
        let editor = editor.read(cx);
        let multibuffer = editor.buffer().read(cx);
        let snapshot = multibuffer.snapshot(cx);
        let selection = editor.selections.newest_anchor();
        let (start_anchor, _) = snapshot.anchor_to_buffer_anchor(selection.start)?;
        let (end_anchor, _) = snapshot.anchor_to_buffer_anchor(selection.end)?;
        if start_anchor.buffer_id != end_anchor.buffer_id {
            return None;
        }
        let buffer = multibuffer.buffer(start_anchor.buffer_id)?;
        let buffer = buffer.read(cx);
        let file = buffer.file()?;
        let path =
            SharedString::from(file.path().as_std_path().to_string_lossy().to_string());
        let buffer_snapshot = buffer.snapshot();
        let start_row = start_anchor.to_point(&buffer_snapshot).row + 1;
        let end_row = end_anchor.to_point(&buffer_snapshot).row + 1;
        let (start_line, line) = if start_row == end_row {
            (None, end_row)
        } else {
            (Some(start_row.min(end_row)), start_row.max(end_row))
        };
        Some((path, start_line, line))
    }

    /// True when a reply composer is open for the given thread root.
    fn is_composing_for(&self, root_id: u64) -> bool {
        self.inline_composers.iter().any(|c| {
            matches!(&c.target, ComposerTarget::Reply { in_reply_to } if *in_reply_to == root_id)
        })
    }

    /// Re-render injected inline comment blocks (after reactions load or toggle)
    /// for whichever diff editor currently has them, deferred out of the click's
    /// layout pass to avoid an editor resize feedback loop.
    fn refresh_inline_comment_blocks(&mut self, cx: &mut Context<Self>) {
        // Prefer the active diff editor (it may not have any injected blocks yet
        // if comments hadn't loaded when it opened), falling back to whichever
        // editor we last injected into.
        let active_diff = self
            ._workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).active_item(cx))
            .and_then(|item| item.act_as::<Editor>(cx))
            .filter(|editor| editor.read(cx).addon::<ReviewEditorAddon>().is_some());
        let editor = active_diff.or_else(|| {
            self.injected_comment_blocks
                .values()
                .find_map(|(editor, _)| editor.upgrade())
        });
        let Some(editor) = editor else {
            return;
        };
        let weak_self = cx.weak_entity();
        cx.defer(move |cx| {
            weak_self
                .update(cx, |this, cx| this.reinject_inline_blocks(&editor, cx))
                .ok();
        });
    }

    fn reinject_inline_blocks(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let Some((review_view, _)) = &self.review_view else {
            return;
        };
        let review_view = review_view.clone();
        self.remove_all_injected_blocks(cx);
        self.inject_for_multibuffer_editor(editor, &review_view, cx);
    }

    fn cancel_inline_composer(&mut self, id: usize, cx: &mut Context<Self>) {
        let Some(pos) = self.inline_composers.iter().position(|c| c.id == id) else {
            return;
        };
        let composer = self.inline_composers.remove(pos);
        let editor = composer.editor;
        let weak_self = cx.weak_entity();
        cx.notify();
        // Defer the block remove/re-inject out of this click's layout pass to
        // avoid a same-frame resize feedback loop in the editor.
        cx.defer(move |cx| {
            let Some(editor) = editor.upgrade() else {
                return;
            };
            weak_self
                .update(cx, |this, cx| this.reinject_inline_blocks(&editor, cx))
                .ok();
        });
    }

    fn submit_inline_composer(&mut self, id: usize, cx: &mut Context<Self>) {
        let Some(composer) = self.inline_composers.iter().find(|c| c.id == id) else {
            return;
        };
        if composer.submitting {
            return;
        }
        let body = composer.input.read(cx).text(cx);
        if body.trim().is_empty() {
            return;
        }
        let target_editor = composer.editor.clone();
        let target = match &composer.target {
            ComposerTarget::Reply { in_reply_to } => Ok(*in_reply_to),
            ComposerTarget::New {
                path,
                commit_id,
                start_line,
                line,
            } => Err((path.to_string(), commit_id.clone(), *start_line, *line)),
        };

        let (Some(provider), Some(owner), Some(repo), Some(pr)) = (
            self.provider.clone(),
            self.remote_owner.clone(),
            self.remote_repo.clone(),
            self.selected_pr.as_ref(),
        ) else {
            return;
        };
        let number = pr.number;

        if let Some(composer) = self.inline_composers.iter_mut().find(|c| c.id == id) {
            composer.submitting = true;
        }
        cx.notify();

        cx.spawn(async move |this, cx| {
            let new_comment = match target {
                Ok(in_reply_to) => {
                    provider
                        .reply_to_comment(&owner, &repo, number, &body, in_reply_to)
                        .await?
                }
                Err((path, commit_id, start_line, line)) => {
                    provider
                        .submit_inline_comment(
                            &owner, &repo, number, &body, &commit_id, &path, start_line, line,
                            "RIGHT",
                        )
                        .await?
                }
            };
            this.update(cx, |this, cx| {
                this.inline_composers.retain(|c| c.id != id);
                if let Some((review_view, _)) = &this.review_view {
                    let review_view = review_view.clone();
                    review_view.update(cx, |review_view, cx| {
                        review_view.add_comment(new_comment, cx);
                    });
                    if let Some(editor) = target_editor.upgrade() {
                        this.reinject_inline_blocks(&editor, cx);
                    }
                }
                cx.notify();
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn build_comment_threads(
        comments: &[ReviewComment],
        cx: &mut App,
    ) -> Vec<(u32, Vec<(ReviewComment, Entity<Markdown>, Vec<SuggestionBlock>)>)> {
        let mut threads: Vec<(
            u32,
            Vec<(ReviewComment, Entity<Markdown>, Vec<SuggestionBlock>)>,
        )> = Vec::new();

        for comment in comments {
            if comment.reply_to.is_none() {
                if let Some(line) = comment.line {
                    let (cleaned_body, suggestions) = parse_suggestions(&comment.body);
                    let md = comment_markdown(SharedString::from(cleaned_body), cx);
                    threads.push((line, vec![(comment.clone(), md, suggestions)]));
                }
            }
        }

        for comment in comments {
            if let Some(reply_to) = comment.reply_to {
                for (_, thread) in &mut threads {
                    if thread.first().map(|(c, _, _)| c.id) == Some(reply_to) {
                        let (cleaned_body, suggestions) = parse_suggestions(&comment.body);
                        let md = comment_markdown(SharedString::from(cleaned_body), cx);
                        thread.push((comment.clone(), md, suggestions));
                        break;
                    }
                }
            }
        }

        threads
    }

    fn estimate_block_height(
        thread: &[(ReviewComment, Entity<Markdown>, Vec<SuggestionBlock>)],
    ) -> u32 {
        let mut total: u32 = 0;
        for (comment, _, suggestions) in thread {
            let line_count = comment.body.lines().count().max(1) as u32;
            total += 1 + line_count + 1;
            for suggestion in suggestions {
                let suggestion_lines = suggestion.suggested_code.lines().count().max(1) as u32;
                total += 2 + suggestion_lines + 1;
            }
        }
        total.max(3)
    }

    fn handle_react_to_comment(&mut self, action: ReactToComment, cx: &mut Context<Self>) {
        let Some(content) = ReactionContent::from_graphql(&action.content) else {
            log::warn!("react_to_comment: unknown content {:?}", action.content);
            return;
        };
        let Some((review_view, _)) = &self.review_view else {
            return;
        };
        review_view.update(cx, |review_view, cx| {
            review_view.toggle_reaction(action.comment_id, content, action.add, cx);
        });
    }

    fn handle_apply_suggestion(
        &mut self,
        comment_id: u64,
        active_editor: Option<Entity<Editor>>,
        cx: &mut Context<Self>,
    ) {
        let Some((review_view, _)) = &self.review_view else {
            log::warn!("apply_suggestion: no review view");
            return;
        };

        let comment = review_view
            .read(cx)
            .pr_comments()
            .iter()
            .find(|c| c.id == comment_id)
            .cloned();

        let Some(comment) = comment else {
            log::warn!("apply_suggestion: comment {} not found", comment_id);
            return;
        };

        let (_, suggestions) = parse_suggestions(&comment.body);
        let Some(suggestion) = suggestions.into_iter().next() else {
            log::warn!("apply_suggestion: no suggestion in comment {}", comment_id);
            return;
        };

        let Some(line) = comment.line else {
            log::warn!("apply_suggestion: comment {} has no line", comment_id);
            return;
        };
        let Some(path) = comment.path.clone() else {
            log::warn!("apply_suggestion: comment {} has no path", comment_id);
            return;
        };

        let Some(editor) = active_editor else {
            log::warn!("apply_suggestion: no active editor");
            return;
        };

        // The comment's line numbers are file-relative (the new/RIGHT side). The
        // active editor may be a multibuffer spanning several files, so resolve
        // the underlying buffer for `path` and edit it directly rather than in
        // multibuffer coordinates.
        let buffers = editor.read(cx).buffer().read(cx).all_buffers();
        let target_buffer = buffers.into_iter().find(|buffer| {
            buffer.read(cx).file().is_some_and(|file| {
                file.path().as_std_path().to_string_lossy() == path.as_ref()
            })
        });
        let Some(target_buffer) = target_buffer else {
            log::warn!("apply_suggestion: buffer for {path} not open in editor");
            return;
        };

        let start_row = comment.start_line.unwrap_or(line).min(line).saturating_sub(1);
        let end_row = line.saturating_sub(1);

        target_buffer.update(cx, |buffer, cx| {
            let snapshot = buffer.snapshot();
            let max_row = snapshot.max_point().row;
            if start_row > max_row {
                log::warn!("apply_suggestion: row {start_row} exceeds buffer max {max_row}");
                return;
            }
            let end_row = end_row.min(max_row);

            let start = Point::new(start_row, 0);
            // Replace whole lines: extend to the start of the line after the
            // range so the old lines (and their newlines) are removed, except at
            // end-of-file where there's no trailing newline to consume.
            let (end, replacement) = if end_row < max_row {
                (Point::new(end_row + 1, 0), format!("{}\n", suggestion.suggested_code))
            } else {
                (snapshot.max_point(), suggestion.suggested_code.clone())
            };

            buffer.edit([(start..end, replacement)], None, cx);
        });
    }

    fn flush_pending_action(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(action) = self.pending_action.take() else {
            return;
        };
        match action {
            PendingAction::OpenDiff(path) => {
                let Some(workspace) = self._workspace.upgrade() else {
                    return;
                };
                let Some(active_repo) = self.active_repository.as_ref() else {
                    return;
                };
                let Some(project_path) =
                    active_repo.read(cx).repo_path_to_project_path(&path, cx)
                else {
                    return;
                };

                if let Some(pr) = self.selected_pr.as_ref() {
                    let base_ref = pr.base_sha.clone();
                    let head_ref = Some(pr.head_sha.clone());
                    let tab_label = Some(SharedString::from(format!("#{}", pr.number)));
                    workspace.update(cx, |workspace, cx| {
                        crate::project_diff::ProjectDiff::deploy_merge_diff(
                            workspace,
                            base_ref,
                            head_ref,
                            Some(project_path),
                            tab_label,
                            // Navigating to a specific file post-load; don't re-hold.
                            false,
                            window,
                            cx,
                        );
                    });
                } else {
                    window.dispatch_action(Box::new(crate::project_diff::BranchDiff), cx);
                }
            }
            PendingAction::SelectPullRequest(pr) => {
                self.create_review_view(&pr, window, cx);
                // Open the diff tab immediately, held behind the loading state, so
                // the loading window appears instantly. It's empty until the ref
                // is fetched; `maybe_open_pr_diff` re-deploys (existing-tab reload)
                // once the tree is ready, and it's revealed once files + comments
                // are loaded.
                if let Some(workspace) = self._workspace.upgrade() {
                    let base_ref = pr.base_sha.clone();
                    let head_ref = Some(pr.head_sha.clone());
                    let tab_label = Some(SharedString::from(format!("#{}", pr.number)));
                    workspace.update(cx, |workspace, cx| {
                        crate::project_diff::ProjectDiff::deploy_merge_diff(
                            workspace, base_ref, head_ref, None, tab_label, true, window, cx,
                        );
                    });
                }
            }
            PendingAction::OpenPrDiff => {
                let Some(pr) = self.selected_pr.as_ref() else {
                    return;
                };
                if let Some(workspace) = self._workspace.upgrade() {
                    let base_ref = pr.base_sha.clone();
                    let head_ref = Some(pr.head_sha.clone());
                    let tab_label = Some(SharedString::from(format!("#{}", pr.number)));
                    workspace.update(cx, |workspace, cx| {
                        // `None` path → opens at Anchor::Min (top of the first
                        // file) with no navigation. Held (review_loading=true)
                        // until files load and comments inject, then revealed.
                        crate::project_diff::ProjectDiff::deploy_merge_diff(
                            workspace, base_ref, head_ref, None, tab_label, true, window, cx,
                        );
                    });
                }
            }
        }
    }
}

impl ReviewPanel {
    /// Forward roving-selection actions to the PR list when it's the active
    /// view. No-ops otherwise, so the bindings are harmless in other views.
    fn with_pull_request_list(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut PullRequestList, &mut Context<PullRequestList>),
    ) {
        if !self.showing_list() {
            return;
        }
        if let Some((pr_list, _)) = &self.pull_request_list {
            pr_list.update(cx, |list, cx| f(list, cx));
        }
    }

    /// Forward file-list navigation to the drilled-in review view.
    fn with_review_view(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut ReviewView, &mut Context<ReviewView>),
    ) {
        if !self.showing_review() {
            return;
        }
        if let Some((review_view, _)) = &self.review_view {
            review_view.update(cx, |view, cx| f(view, cx));
        }
    }

    fn select_next_pr(&mut self, _: &menu::SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
        self.with_pull_request_list(cx, |list, cx| list.select_next(cx));
        self.with_review_view(cx, |view, cx| view.select_next_file(cx));
    }

    fn select_previous_pr(
        &mut self,
        _: &menu::SelectPrevious,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.with_pull_request_list(cx, |list, cx| list.select_previous(cx));
        self.with_review_view(cx, |view, cx| view.select_previous_file(cx));
    }

    fn select_first_pr(
        &mut self,
        _: &menu::SelectFirst,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.with_pull_request_list(cx, |list, cx| list.select_first(cx));
        self.with_review_view(cx, |view, cx| view.select_first_file(cx));
    }

    fn select_last_pr(&mut self, _: &menu::SelectLast, _window: &mut Window, cx: &mut Context<Self>) {
        self.with_pull_request_list(cx, |list, cx| list.select_last(cx));
        self.with_review_view(cx, |view, cx| view.select_last_file(cx));
    }

    fn confirm_pr(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        self.with_pull_request_list(cx, |list, cx| list.confirm_selected(cx));
        self.with_review_view(cx, |view, cx| view.open_selected_file(cx));
    }

    fn toggle_viewed(&mut self, _: &ToggleViewed, _window: &mut Window, cx: &mut Context<Self>) {
        self.with_review_view(cx, |view, cx| view.toggle_selected_file_viewed(cx));
    }
}

impl Render for ReviewPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.flush_pending_action(window, cx);
        let header = self.render_header(window, cx);
        let loading = || {
            v_flex()
                .size_full()
                .justify_center()
                .items_center()
                .child(Label::new("Loading…").color(Color::Muted))
        };
        v_flex()
            .id("review_panel")
            .key_context("PullRequestPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next_pr))
            .on_action(cx.listener(Self::select_previous_pr))
            .on_action(cx.listener(Self::select_first_pr))
            .on_action(cx.listener(Self::select_last_pr))
            .on_action(cx.listener(Self::confirm_pr))
            .on_action(cx.listener(Self::toggle_viewed))
            .size_full()
            .children(header)
            .map(|parent| match self.active_tab {
                ReviewPanelTab::PullRequests if self.reviewing => match &self.review_view {
                    Some((review_view, _)) => parent.child(review_view.clone()),
                    None => parent.child(loading()),
                },
                ReviewPanelTab::PullRequests => match &self.pull_request_list {
                    Some((pr_list, _)) => parent.child(pr_list.clone()),
                    None => parent.child(loading()),
                },
                ReviewPanelTab::Configuration => match &self.configuration_view {
                    Some((configuration_view, _)) => parent.child(configuration_view.clone()),
                    None => parent.child(loading()),
                },
            })
    }
}

impl Focusable for ReviewPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ReviewPanelEvent> for ReviewPanel {}

/// Render an inline comment thread block with a trailing Reply button that
/// opens the reply composer for the thread's root comment.
fn render_comment_thread_with_reply(
    thread: Vec<(ReviewComment, Entity<Markdown>, Vec<SuggestionBlock>)>,
    root_id: Option<u64>,
    weak_panel: WeakEntity<ReviewPanel>,
    weak_editor: WeakEntity<Editor>,
    cx: &mut BlockContext,
) -> AnyElement {
    let colors = cx.theme().colors().clone();
    // Match render_pr_comment_block's inset so the composer lines up under the
    // comment text rather than spanning the gutter.
    let anchor_x = cx.anchor_x;
    let max_width = cx.max_width;
    let body = render_pr_comment_block(thread, cx);
    // Fill the whole block (gutter included) with the editor background so the
    // diff's added/removed gutter bar doesn't bleed through and make the
    // comment/composer look like part of the code below.
    let mut container = v_flex()
        .w_full()
        .bg(colors.editor_background)
        .child(body);

    // If the reply composer is open for this thread, render it inline; else show
    // a Reply button that opens it.
    let composer = root_id.and_then(|root_id| {
        weak_panel.upgrade().and_then(|panel| {
            let panel = panel.read(cx);
            panel
                .inline_composers
                .iter()
                .find(|composer| {
                    matches!(&composer.target, ComposerTarget::Reply { in_reply_to } if *in_reply_to == root_id)
                })
                .map(|composer| (composer.input.clone(), composer.submitting, composer.id))
        })
    });

    if let Some((input, submitting, composer_id)) = composer {
        container = container.child(
            v_flex()
                .max_w(max_width - anchor_x)
                .pl(anchor_x)
                .pr_2()
                .pb_2()
                .pt_1()
                .child(ComposerBubble {
                    input,
                    composer_id,
                    submit_label: "Reply".into(),
                    submitting,
                    panel: weak_panel,
                }),
        );
    } else if let Some(root_id) = root_id {
        // A persistent muted "Reply…" field (reads as an input affordance, not a
        // heading) that expands the composer in place when clicked.
        container = container.child(
            h_flex().pl(anchor_x).pr_2().pb_1().child(
                div()
                    .id(("inline-reply", root_id as usize))
                    .w_full()
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border_variant)
                    .cursor_pointer()
                    .hover(|style| style.bg(colors.ghost_element_hover))
                    .child(
                        Label::new("Reply…")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .on_click(move |_, window, cx| {
                        let weak_editor = weak_editor.clone();
                        weak_panel
                            .update(cx, |panel, cx| {
                                panel.begin_inline_reply(weak_editor, root_id, window, cx);
                            })
                            .ok();
                    }),
            ),
        );
    }
    container.into_any_element()
}

/// The bordered input bubble shared by the new-comment and reply composers: the
/// text input plus Cancel/submit buttons. Centralizes the arrow-key handling so
/// Move{Up,Down} (which editors propagate at the first/last line) stay inside the
/// composer instead of bubbling to the parent diff editor and navigating the PR.
#[derive(IntoElement)]
struct ComposerBubble {
    input: Entity<Editor>,
    composer_id: usize,
    submit_label: SharedString,
    submitting: bool,
    panel: WeakEntity<ReviewPanel>,
}

impl RenderOnce for ComposerBubble {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let id = self.composer_id;
        let submit_panel = self.panel.clone();
        let action_panel = self.panel.clone();
        let cancel_panel = self.panel;

        // `cmd-enter` (bound to SubmitComment in the PullRequestComposer key context)
        // submits without leaving the editor.
        let submit_hint = KeybindingHint::new(
            ui::KeyBinding::for_action(&SubmitComment, cx),
            colors.element_background,
        )
        .suffix(self.submit_label.clone());

        v_flex()
            .w_full()
            .min_w_0()
            .px_2()
            .py_1()
            .gap_2()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .bg(colors.element_background)
            .key_context("PullRequestComposer")
            .on_action(move |_: &SubmitComment, _window, cx| {
                action_panel
                    .update(cx, |panel, cx| panel.submit_inline_composer(id, cx))
                    .ok();
            })
            .on_action(|_: &zed_actions::editor::MoveUp, _window, _cx| {})
            .on_action(|_: &zed_actions::editor::MoveDown, _window, _cx| {})
            .child(self.input)
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .gap_1()
                    .child(submit_hint)
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new(
                                    SharedString::from(format!("composer-cancel-{id}")),
                                    "Cancel",
                                )
                                .size(ButtonSize::Compact)
                                .label_size(LabelSize::Small)
                                .on_click(move |_, _window, cx| {
                                    cancel_panel
                                        .update(cx, |panel, cx| panel.cancel_inline_composer(id, cx))
                                        .ok();
                                }),
                            )
                            .child(
                                Button::new(
                                    SharedString::from(format!("composer-submit-{id}")),
                                    self.submit_label,
                                )
                                .size(ButtonSize::Compact)
                                .label_size(LabelSize::Small)
                                .style(ui::ButtonStyle::Outlined)
                                .disabled(self.submitting)
                                .on_click(move |_, _window, cx| {
                                    submit_panel
                                        .update(cx, |panel, cx| panel.submit_inline_composer(id, cx))
                                        .ok();
                                }),
                            ),
                    ),
            )
    }
}

/// Render a standalone new-comment composer block.
fn render_composer_block(
    input: Entity<Editor>,
    composer_id: usize,
    weak_panel: WeakEntity<ReviewPanel>,
    cx: &mut BlockContext,
) -> AnyElement {
    let colors = cx.theme().colors().clone();
    let anchor_x = cx.anchor_x;
    let submitting = weak_panel
        .upgrade()
        .and_then(|panel| {
            panel
                .read(cx)
                .inline_composers
                .iter()
                .find(|c| c.id == composer_id)
                .map(|c| c.submitting)
        })
        .unwrap_or(false);

    v_flex()
        .w_full()
        .overflow_x_hidden()
        .pl(anchor_x)
        .pr_2()
        .py_2()
        .bg(colors.editor_background)
        .child(ComposerBubble {
            input,
            composer_id,
            submit_label: "Comment".into(),
            submitting,
            panel: weak_panel,
        })
        .into_any_element()
}

fn parse_github_remote(url: &str) -> anyhow::Result<(String, String)> {
    if let Some(rest) = url.strip_prefix("git@github.com:") {
        let rest = rest.trim_end_matches(".git");
        let parts: Vec<&str> = rest.splitn(2, '/').collect();
        if parts.len() == 2 {
            return Ok((parts[0].to_string(), parts[1].to_string()));
        }
    }

    if let Some(rest) = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
    {
        let rest = rest.trim_end_matches(".git");
        let parts: Vec<&str> = rest.splitn(2, '/').collect();
        if parts.len() == 2 {
            return Ok((parts[0].to_string(), parts[1].to_string()));
        }
    }

    anyhow::bail!("Could not parse GitHub owner/repo from remote URL: {}", url)
}
