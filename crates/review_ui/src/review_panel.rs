use crate::file_list::{FileList, FileListEvent};
use crate::github_provider::GitHubProvider;
use crate::inline_comment::{ApplySuggestion, ReactToComment, comment_markdown, parse_suggestions, render_pr_comment_block, SuggestionBlock};
use crate::pull_request_list::{PullRequestList, PullRequestListEvent, RemoteState};
use crate::review_view::{ReviewView, ReviewViewEvent};
use crate::github_token::resolve_github_token;
use crate::review_panel_settings::ReviewPanelSettings;
use crate::review_provider::{PullRequestInfo, ReactionContent, ReviewComment, ReviewProvider};
use markdown::Markdown;
use anyhow::Result;
use collections::HashMap;
use editor::display_map::{BlockContext, BlockPlacement, BlockProperties, BlockStyle, CustomBlockId};
use editor::{Addon, Editor};
use multi_buffer::ExcerptBoundaryInfo;
use fs::Fs;
use git::repository::RepoPath;
use git::status::{DiffTreeType, TreeDiff};
use gpui::{
    AnyElement, App, AsyncWindowContext, Context, Entity, EntityId, EventEmitter, FocusHandle,
    Focusable, Pixels, Render, SharedString, Subscription, WeakEntity, Window,
};
use text::{Point, ToPoint};
use http_client::HttpClient;
use project::{
    Project,
    git_store::{GitStoreEvent, Repository, RepositoryEvent},
};
use settings::{self, Settings};
use std::sync::Arc;
use ui::{
    Button, ButtonSize, Checkbox, Color, ContextMenu, DynamicSpacing, IconButton, IconName,
    IconSize, IntoElement, Label, LabelSize, PopoverMenu, PopoverMenuHandle, Tab, ToggleState,
    Tooltip, div, h_flex, prelude::*, v_flex,
};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};
use zed_actions::review_panel::{AddComment, ToggleFocus};

const REVIEW_PANEL_KEY: &str = "ReviewPanel";

#[derive(Clone)]
struct RecentReview {
    base_branch: SharedString,
    head_branch: SharedString,
    file_count: usize,
    pull_request: Option<PullRequestInfo>,
}

enum ActiveView {
    Empty,
    PullRequestList,
    ReviewThread,
    FileList,
    Configuration,
}

enum PendingAction {
    OpenDiff(RepoPath),
    OpenLocal(RepoPath),
    SelectPullRequest(PullRequestInfo),
}

pub struct ReviewPanel {
    _workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    active_repository: Option<Entity<Repository>>,
    base_branch: Option<SharedString>,
    head_branch: Option<SharedString>,
    tree_diff: Option<TreeDiff>,
    file_list: Option<(Entity<FileList>, Subscription)>,
    review_view: Option<(Entity<ReviewView>, Subscription)>,
    focus_handle: FocusHandle,
    recent_reviews_menu_handle: PopoverMenuHandle<ContextMenu>,
    options_menu_handle: PopoverMenuHandle<ContextMenu>,
    fs: Arc<dyn Fs>,
    active_view: ActiveView,
    recent_reviews: Vec<RecentReview>,
    http_client: Arc<dyn HttpClient>,
    pull_request_list: Option<(Entity<PullRequestList>, Subscription)>,
    provider: Option<Arc<dyn ReviewProvider>>,
    remote_owner: Option<String>,
    remote_repo: Option<String>,
    selected_pr: Option<PullRequestInfo>,
    pr_ref_fetch_task: Option<gpui::Task<Result<()>>>,
    pending_action: Option<PendingAction>,
    /// Whether the PR's diff has been auto-opened for the current selection.
    pr_diff_opened: bool,
    injected_comment_blocks: HashMap<EntityId, (WeakEntity<Editor>, Vec<CustomBlockId>)>,
    /// The currently open inline reply composer, if any.
    inline_composer: Option<InlineComposer>,
    _workspace_subscription: Option<Subscription>,
}

/// State for an open inline composer (reply to a thread, or a new comment on a
/// line). Both are rendered as blocks produced by the single inject pass, so no
/// separate `insert_blocks` call is made (which previously panicked block_map).
struct InlineComposer {
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

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
        workspace.toggle_panel_focus::<ReviewPanel>(window, cx);
    });

    workspace.register_action(|workspace, action: &ApplySuggestion, _window, cx| {
        let comment_id = action.comment_id;
        let Some(panel) = workspace.panel::<ReviewPanel>(cx) else {
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
        let Some(panel) = workspace.panel::<ReviewPanel>(cx) else {
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
        let Some(panel) = workspace.panel::<ReviewPanel>(cx) else {
            return;
        };
        let action = action.clone();
        panel.update(cx, |panel, cx| {
            panel.handle_react_to_comment(action, cx);
        });
    });
}

impl ReviewPanel {
    pub fn new(
        workspace: &Workspace,
        weak_workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let fs = workspace.app_state().fs.clone();
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
            file_list: None,
            review_view: None,
            focus_handle: cx.focus_handle(),
            fs,
            recent_reviews_menu_handle: PopoverMenuHandle::default(),
            options_menu_handle: PopoverMenuHandle::default(),
            active_view: ActiveView::PullRequestList,
            recent_reviews: Vec::new(),
            http_client: workspace.client().http_client(),
            pull_request_list: None,
            provider: None,
            remote_owner: None,
            remote_repo: None,
            selected_pr: None,
            pr_ref_fetch_task: None,
            pending_action: None,
            pr_diff_opened: false,
            injected_comment_blocks: HashMap::default(),
            inline_composer: None,
            _workspace_subscription: workspace_subscription,
        };
        // Create the PR list up front so pull requests start loading in the
        // background as soon as the provider resolves, before the view is opened.
        this.ensure_pull_request_list(window, cx);
        this.initialize_provider(cx);
        this.load_branches(cx);
        this
    }

    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            let weak_workspace = workspace.weak_handle();
            cx.new(|cx| ReviewPanel::new(workspace, weak_workspace, window, cx))
        })
    }

    fn set_active_view(&mut self, new_view: ActiveView, cx: &mut Context<Self>) {
        self.active_view = new_view;
        cx.notify();
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
                IconButton::new("review-nav-menu", IconName::MenuAltTemp)
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
        let show_tree_toggle = matches!(
            self.active_view,
            ActiveView::FileList | ActiveView::ReviewThread
        );
        let is_tree_view = match &self.active_view {
            ActiveView::FileList => self
                .file_list
                .as_ref()
                .map(|(fl, _)| fl.read(cx).is_tree_view())
                .unwrap_or(false),
            ActiveView::ReviewThread => self
                .review_view
                .as_ref()
                .map(|(rv, _)| rv.read(cx).is_tree_view())
                .unwrap_or(false),
            _ => false,
        };
        let is_review_thread = matches!(self.active_view, ActiveView::ReviewThread);

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
                                move |window, cx| {
                                    weak_panel
                                        .update(cx, |this, cx| {
                                            if is_review_thread {
                                                if let Some((review_view, _)) = &this.review_view {
                                                    review_view.update(cx, |rv, cx| {
                                                        rv.toggle_tree_view(cx);
                                                    });
                                                }
                                            } else if let Some((file_list, _)) = &this.file_list {
                                                file_list.update(cx, |fl, cx| {
                                                    fl.toggle_tree_view(window, cx);
                                                });
                                            }
                                        })
                                        .ok();
                                }
                            },
                        )
                        .separator()
                    })
                    .entry("Configuration", None, |_window, _cx| {
                        // TODO: dispatch OpenConfiguration action
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

    fn render_toolbar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .id("review-panel-toolbar")
            .h(Tab::container_height(cx))
            .max_w_full()
            .flex_none()
            .justify_between()
            .gap_2()
            .bg(cx.theme().colors().tab_bar_background)
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .size_full()
                    .gap(DynamicSpacing::Base04.rems(cx))
                    .pl(DynamicSpacing::Base04.rems(cx))
                    .child(Label::new("Review").size(LabelSize::Small)),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap(DynamicSpacing::Base02.rems(cx))
                    .pr(DynamicSpacing::Base06.rems(cx))
                    .child(
                        IconButton::new("new-review", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("New Review"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show_pull_request_list(window, cx);
                            })),
                    )
                    .child(self.render_recent_reviews_menu(cx))
                    .child(self.render_options_menu(window, cx)),
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

        self.fetch_pr_ref(pr.number, cx);
        self.pending_action = Some(PendingAction::SelectPullRequest(pr.clone()));
        self.set_active_view(ActiveView::ReviewThread, cx);
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
                    ReviewViewEvent::Back => {
                        this.selected_pr = None;
                        this.review_view = None;
                        this.remove_all_injected_blocks(cx);
                        this.show_pull_request_list(window, cx);
                    }
                    ReviewViewEvent::CommentsChanged => {
                        this.refresh_inline_comment_blocks(cx);
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

                // Auto-open the PR's combined diff once, navigated to the first
                // changed file. Opening via a concrete file (rather than the
                // whole diff with no path) gives the editor a resolvable initial
                // selection, avoiding a fold/selection panic.
                if this.selected_pr.is_some() && !this.pr_diff_opened {
                    if let Some(first_path) = this
                        .tree_diff
                        .as_ref()
                        .and_then(|d| d.entries.keys().min().cloned())
                    {
                        this.pr_diff_opened = true;
                        this.pending_action = Some(PendingAction::OpenDiff(first_path));
                    }
                }
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn show_file_list(&mut self, cx: &mut Context<Self>) {
        let file_list = cx.new(|cx| {
            FileList::new(
                self.base_branch.clone(),
                self.head_branch.clone(),
                self.tree_diff.as_ref(),
                cx,
            )
        });
        let subscription = cx.subscribe(&file_list, |this, _file_list, event, cx| match event {
            FileListEvent::OpenFileDiff(path) => {
                this.open_file_diff(path.clone(), cx);
            }
            FileListEvent::OpenLocalFile(path) => {
                this.open_local_file_by_path(path.clone(), cx);
            }
        });
        self.file_list = Some((file_list, subscription));
        self.active_view = ActiveView::FileList;
        cx.notify();
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

        self.active_view = ActiveView::PullRequestList;
        cx.notify();
    }

    fn open_file_diff(&mut self, path: RepoPath, cx: &mut Context<Self>) {
        if let Some((file_list, _)) = &self.file_list {
            file_list.update(cx, |fl, cx| {
                fl.mark_viewed(path.clone(), cx);
                fl.select_path(&path, cx);
            });
        }

        self.pending_action = Some(PendingAction::OpenDiff(path));
        cx.notify();
    }

    fn open_local_file_by_path(&mut self, path: RepoPath, cx: &mut Context<Self>) {
        self.pending_action = Some(PendingAction::OpenLocal(path));
        cx.notify();
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

        // A pending new-comment composer is emitted in the same insert pass.
        if let Some(composer) = &self.inline_composer {
            if let ComposerTarget::New { path, line, .. } = &composer.target {
                let row = line.saturating_sub(1);
                let anchor = multibuffer.read(cx).all_buffers().into_iter().find_map(|buffer| {
                    let buffer = buffer.read(cx);
                    let file = buffer.file()?;
                    let file_path = SharedString::from(
                        file.path().as_std_path().to_string_lossy().to_string(),
                    );
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
                    blocks.push(BlockProperties {
                        placement: BlockPlacement::Below(anchor),
                        height: Some(6),
                        style: BlockStyle::Sticky,
                        render: Arc::new(move |cx| {
                            render_composer_block(input.clone(), weak_panel.clone(), cx)
                        }),
                        priority: 1,
                    });
                }
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
        let composing_root = self.inline_composer.as_ref().and_then(|composer| {
            if let ComposerTarget::Reply { in_reply_to } = &composer.target {
                Some(*in_reply_to)
            } else {
                None
            }
        });
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
                    let composing = root_id.is_some() && root_id == composing_root;
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

        self.inline_composer = Some(InlineComposer {
            editor: target_editor,
            input,
            submitting: false,
            target: ComposerTarget::Reply { in_reply_to },
        });
        self.reinject_inline_blocks(&editor, cx);
        cx.notify();
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

        self.inline_composer = Some(InlineComposer {
            editor: target_editor,
            input,
            submitting: false,
            target: ComposerTarget::New {
                path,
                commit_id,
                start_line,
                line,
            },
        });
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

    /// True when the reply composer is open for the given thread root.
    fn is_composing_for(&self, root_id: u64) -> bool {
        matches!(
            self.inline_composer.as_ref().map(|c| &c.target),
            Some(ComposerTarget::Reply { in_reply_to }) if *in_reply_to == root_id
        )
    }

    /// Re-render injected inline comment blocks (after reactions load or toggle)
    /// for whichever diff editor currently has them, deferred out of the click's
    /// layout pass to avoid an editor resize feedback loop.
    fn refresh_inline_comment_blocks(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self
            .injected_comment_blocks
            .values()
            .find_map(|(editor, _)| editor.upgrade())
        else {
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

    fn cancel_inline_composer(&mut self, cx: &mut Context<Self>) {
        let Some(composer) = self.inline_composer.take() else {
            return;
        };
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

    fn submit_inline_composer(&mut self, cx: &mut Context<Self>) {
        let Some(composer) = self.inline_composer.as_ref() else {
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

        if let Some(composer) = self.inline_composer.as_mut() {
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
                this.inline_composer = None;
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
                    workspace.update(cx, |workspace, cx| {
                        git_ui::project_diff::ProjectDiff::deploy_merge_diff(
                            workspace,
                            base_ref,
                            head_ref,
                            Some(project_path),
                            window,
                            cx,
                        );
                    });
                } else {
                    window.dispatch_action(Box::new(git_ui::project_diff::BranchDiff), cx);
                }
            }
            PendingAction::OpenLocal(path) => {
                let Some(active_repo) = self.active_repository.as_ref() else {
                    return;
                };
                let Some(project_path) =
                    active_repo.read(cx).repo_path_to_project_path(&path, cx)
                else {
                    return;
                };
                let Some(workspace) = self._workspace.upgrade() else {
                    return;
                };
                workspace.update(cx, |workspace, cx| {
                    workspace
                        .open_path_preview(project_path, None, true, false, true, window, cx)
                        .detach_and_log_err(cx);
                });
            }
            PendingAction::SelectPullRequest(pr) => {
                self.create_review_view(&pr, window, cx);
                // Open the combined diff tab immediately (empty until the ref
                // finishes fetching, after which load_diff's completion re-deploys
                // to reload + navigate). Safe now that PR diffs don't auto-fold.
                if let Some(workspace) = self._workspace.upgrade() {
                    let base_ref = pr.base_sha.clone();
                    let head_ref = Some(pr.head_sha.clone());
                    workspace.update(cx, |workspace, cx| {
                        git_ui::project_diff::ProjectDiff::deploy_merge_diff(
                            workspace, base_ref, head_ref, None, window, cx,
                        );
                    });
                }
            }
        }
    }
}

impl Render for ReviewPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.flush_pending_action(window, cx);
        v_flex()
            .id("review_panel")
            .track_focus(&self.focus_handle)
            .size_full()
            .child(self.render_toolbar(window, cx))
            .map(|parent| match &self.active_view {
                ActiveView::Empty => parent.child(
                    v_flex()
                        .size_full()
                        .justify_center()
                        .items_center()
                        .child(Label::new("No review selected").color(Color::Muted)),
                ),
                ActiveView::PullRequestList => {
                    if let Some((pr_list, _)) = &self.pull_request_list {
                        parent.child(pr_list.clone())
                    } else {
                        parent.child(
                            v_flex()
                                .size_full()
                                .justify_center()
                                .items_center()
                                .child(Label::new("Loading...").color(Color::Muted)),
                        )
                    }
                }
                ActiveView::ReviewThread => {
                    if let Some((review_view, _)) = &self.review_view {
                        parent.child(review_view.clone())
                    } else {
                        parent.child(
                            v_flex()
                                .size_full()
                                .justify_center()
                                .items_center()
                                .child(Label::new("Loading...").color(Color::Muted)),
                        )
                    }
                }
                ActiveView::FileList => {
                    if let Some((file_list, _)) = &self.file_list {
                        parent.child(file_list.clone())
                    } else {
                        parent.child(
                            v_flex()
                                .size_full()
                                .justify_center()
                                .items_center()
                                .child(Label::new("Loading...").color(Color::Muted)),
                        )
                    }
                }
                ActiveView::Configuration => parent.child(
                    v_flex()
                        .size_full()
                        .justify_center()
                        .items_center()
                        .child(Label::new("Configuration (coming soon)").color(Color::Muted)),
                ),
            })
    }
}

impl Focusable for ReviewPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for ReviewPanel {}

impl Panel for ReviewPanel {
    fn persistent_name() -> &'static str {
        "ReviewPanel"
    }

    fn panel_key() -> &'static str {
        REVIEW_PANEL_KEY
    }

    fn position(&self, _window: &Window, cx: &App) -> DockPosition {
        ReviewPanelSettings::get_global(cx).dock
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        settings::update_settings_file(self.fs.clone(), cx, move |settings, _| {
            settings.review_panel.get_or_insert_default().dock = Some(position.into())
        });
    }

    fn default_size(&self, _window: &Window, cx: &App) -> Pixels {
        ReviewPanelSettings::get_global(cx).default_width
    }

    fn icon(&self, _window: &Window, cx: &App) -> Option<ui::IconName> {
        Some(ui::IconName::PullRequest).filter(|_| ReviewPanelSettings::get_global(cx).button)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Review Panel")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        10
    }
}

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
                .inline_composer
                .as_ref()
                .filter(|composer| {
                    matches!(&composer.target, ComposerTarget::Reply { in_reply_to } if *in_reply_to == root_id)
                })
                .map(|composer| (composer.input.clone(), composer.submitting))
        })
    });

    if let Some((input, submitting)) = composer {
        let cancel_panel = weak_panel.clone();
        container = container.child(
            v_flex()
                .max_w(max_width - anchor_x)
                .pl(anchor_x)
                .pr_2()
                .pb_2()
                .pt_1()
                .gap_2()
                .child(
                    div()
                        .w_full()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .border_1()
                        .border_color(colors.border_variant)
                        .bg(colors.element_background)
                        .child(input),
                )
                .child(
                    h_flex()
                        .justify_end()
                        .gap_1()
                        .child(
                            Button::new("inline-composer-cancel", "Cancel")
                                .size(ButtonSize::Compact)
                                .label_size(LabelSize::Small)
                                .on_click(move |_, _window, cx| {
                                    cancel_panel
                                        .update(cx, |panel, cx| panel.cancel_inline_composer(cx))
                                        .ok();
                                }),
                        )
                        .child(
                            Button::new("inline-composer-submit", "Reply")
                                .size(ButtonSize::Compact)
                                .label_size(LabelSize::Small)
                                .style(ui::ButtonStyle::Filled)
                                .disabled(submitting)
                                .on_click(move |_, _window, cx| {
                                    weak_panel
                                        .update(cx, |panel, cx| panel.submit_inline_composer(cx))
                                        .ok();
                                }),
                        ),
                ),
        );
    } else if let Some(root_id) = root_id {
        container = container.child(
            h_flex().pl(anchor_x).pb_1().child(
                Button::new(("inline-reply", root_id as usize), "Reply")
                    .size(ButtonSize::Compact)
                    .label_size(LabelSize::Small)
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

/// Render a standalone new-comment composer block (text input + Cancel/Comment).
fn render_composer_block(
    input: Entity<Editor>,
    weak_panel: WeakEntity<ReviewPanel>,
    cx: &mut BlockContext,
) -> AnyElement {
    let colors = cx.theme().colors().clone();
    let anchor_x = cx.anchor_x;
    let submitting = weak_panel
        .upgrade()
        .and_then(|panel| panel.read(cx).inline_composer.as_ref().map(|c| c.submitting))
        .unwrap_or(false);
    let cancel_panel = weak_panel.clone();

    v_flex()
        .w_full()
        .overflow_x_hidden()
        .pl(anchor_x)
        .pr_2()
        .py_2()
        .gap_2()
        .bg(colors.editor_background)
        .child(
            div()
                .w_full()
                .min_w_0()
                .px_2()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(colors.border_variant)
                .bg(colors.element_background)
                .child(input),
        )
        .child(
            h_flex()
                .justify_end()
                .gap_1()
                .child(
                    Button::new("new-comment-cancel", "Cancel")
                        .size(ButtonSize::Compact)
                        .label_size(LabelSize::Small)
                        .on_click(move |_, _window, cx| {
                            cancel_panel
                                .update(cx, |panel, cx| panel.cancel_inline_composer(cx))
                                .ok();
                        }),
                )
                .child(
                    Button::new("new-comment-submit", "Comment")
                        .size(ButtonSize::Compact)
                        .label_size(LabelSize::Small)
                        .style(ui::ButtonStyle::Filled)
                        .disabled(submitting)
                        .on_click(move |_, _window, cx| {
                            weak_panel
                                .update(cx, |panel, cx| panel.submit_inline_composer(cx))
                                .ok();
                        }),
                ),
        )
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
