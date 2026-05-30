use anyhow::Result;
use buffer_diff::BufferDiff;
use collections::HashSet;
use futures::StreamExt;
use git::{
    repository::RepoPath,
    status::{DiffTreeType, FileStatus, StatusCode, TrackedStatus, TreeDiff, TreeDiffStatus},
};
use gpui::{
    App, AppContext as _, AsyncApp, AsyncWindowContext, Context, Entity, EventEmitter, SharedString,
    Subscription, Task, WeakEntity, Window,
};

use language::{
    Buffer, Capability, DiskState, LanguageRegistry, LineEnding, ReplicaId, Rope, TextBuffer,
};
use std::path::PathBuf;
use std::sync::Arc;
use text::BufferId;
use util::ResultExt;
use util::{paths::PathStyle, rel_path::RelPath};
use ztracing::instrument;

use crate::{
    Project, WorktreeId,
    git_store::{GitStoreEvent, Repository, RepositoryEvent},
};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DiffBase {
    Head,
    Merge {
        base_ref: SharedString,
        #[serde(default)]
        head_ref: Option<SharedString>,
    },
}

impl DiffBase {
    pub fn is_merge_base(&self) -> bool {
        matches!(self, DiffBase::Merge { .. })
    }

    pub fn has_explicit_head(&self) -> bool {
        matches!(self, DiffBase::Merge { head_ref: Some(_), .. })
    }
}

pub struct BranchDiff {
    diff_base: DiffBase,
    repo: Option<Entity<Repository>>,
    project: Entity<Project>,
    base_commit: Option<SharedString>,
    head_commit: Option<SharedString>,
    tree_diff: Option<TreeDiff>,
    tree_diff_update_needed: bool,
    tree_diff_base_task: Option<Task<()>>,
    _subscription: Subscription,
    update_needed: postage::watch::Sender<()>,
    _task: Task<()>,
}

pub enum BranchDiffEvent {
    FileListChanged,
    DiffBaseChanged,
}

impl EventEmitter<BranchDiffEvent> for BranchDiff {}

impl BranchDiff {
    pub fn new(
        source: DiffBase,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let git_store = project.read(cx).git_store().clone();
        let repo = git_store.read(cx).active_repository();
        let git_store_subscription = cx.subscribe_in(
            &git_store,
            window,
            move |this, _git_store, event, _window, cx| {
                let should_update = match event {
                    GitStoreEvent::ActiveRepositoryChanged(new_repo_id) => {
                        this.repo.is_none() && new_repo_id.is_some()
                    }
                    GitStoreEvent::RepositoryUpdated(
                        event_repo_id,
                        RepositoryEvent::StatusesChanged | RepositoryEvent::HeadChanged,
                        _,
                    ) => this
                        .repo
                        .as_ref()
                        .is_some_and(|r| r.read(cx).snapshot().id == *event_repo_id),
                    GitStoreEvent::ConflictsUpdated => this.repo.is_some(),
                    _ => false,
                };

                if should_update {
                    cx.emit(BranchDiffEvent::FileListChanged);
                    *this.update_needed.borrow_mut() = ();
                }
            },
        );

        let (send, recv) = postage::watch::channel::<()>();
        let worker = window.spawn(cx, {
            let this = cx.weak_entity();
            async |cx| Self::handle_status_updates(this, recv, cx).await
        });

        Self {
            diff_base: source,
            repo,
            project,
            tree_diff: None,
            tree_diff_update_needed: false,
            tree_diff_base_task: None,
            base_commit: None,
            head_commit: None,
            _subscription: git_store_subscription,
            _task: worker,
            update_needed: send,
        }
    }

    pub fn diff_base(&self) -> &DiffBase {
        &self.diff_base
    }

    pub fn set_repo(&mut self, repo: Option<Entity<Repository>>, cx: &mut Context<Self>) {
        let same_repo = match (self.repo.as_ref(), repo.as_ref()) {
            (Some(current), Some(new)) => current.read(cx).id == new.read(cx).id,
            (None, None) => true,
            _ => false,
        };
        if same_repo {
            return;
        }

        self.repo = repo;
        self.tree_diff = None;
        self.tree_diff_update_needed = self.diff_base.is_merge_base();
        self.tree_diff_base_task = None;
        self.base_commit = None;
        self.head_commit = None;
        cx.emit(BranchDiffEvent::FileListChanged);
        *self.update_needed.borrow_mut() = ();
    }

    pub fn set_diff_base(&mut self, diff_base: DiffBase, cx: &mut Context<Self>) {
        if self.diff_base == diff_base {
            return;
        }

        self.tree_diff_update_needed = diff_base.is_merge_base();
        self.tree_diff = None;
        self.tree_diff_base_task = None;
        self.diff_base = diff_base;
        self.base_commit = None;
        self.head_commit = None;

        cx.emit(BranchDiffEvent::DiffBaseChanged);
        if self.tree_diff_update_needed {
            *self.update_needed.borrow_mut() = ();
        }
    }

    pub async fn handle_status_updates(
        this: WeakEntity<Self>,
        mut recv: postage::watch::Receiver<()>,
        cx: &mut AsyncWindowContext,
    ) {
        this.update(cx, |this, cx| this.spawn_reload_tree_diff(cx))
            .log_err();
        while recv.next().await.is_some() {
            let Ok(()) = this.update(cx, |this, cx| {
                let mut needs_update = this.tree_diff_update_needed;
                this.tree_diff_update_needed = false;

                if this.repo.is_none() {
                    let active_repo = this
                        .project
                        .read(cx)
                        .git_store()
                        .read(cx)
                        .active_repository();
                    if active_repo.is_some() {
                        this.repo = active_repo;
                        needs_update = true;
                    }
                } else if let Some(repo) = this.repo.as_ref() {
                    repo.update(cx, |repo, _| {
                        if let Some(branch) = &repo.branch
                            && let DiffBase::Merge { base_ref, .. } = &this.diff_base
                            && let Some(commit) = branch.most_recent_commit.as_ref()
                            && &branch.ref_name == base_ref
                            && this.base_commit.as_ref() != Some(&commit.sha)
                        {
                            this.base_commit = Some(commit.sha.clone());
                            needs_update = true;
                        }

                        if repo.head_commit.as_ref().map(|c| &c.sha) != this.head_commit.as_ref() {
                            this.head_commit = repo.head_commit.as_ref().map(|c| c.sha.clone());
                            needs_update = true;
                        }
                    })
                }

                if needs_update {
                    this.spawn_reload_tree_diff(cx);
                }
            }) else {
                return;
            };
        }
    }

    pub fn status_for_buffer_id(&self, buffer_id: BufferId, cx: &App) -> Option<FileStatus> {
        let (repo, path) = self
            .project
            .read(cx)
            .git_store()
            .read(cx)
            .repository_and_path_for_buffer_id(buffer_id, cx)?;
        if self.repo() == Some(&repo) {
            return self.merge_statuses(
                repo.read(cx)
                    .status_for_path(&path)
                    .map(|status| status.status),
                self.tree_diff
                    .as_ref()
                    .and_then(|diff| diff.entries.get(&path)),
            );
        }
        None
    }

    pub fn merge_statuses(
        &self,
        diff_from_head: Option<FileStatus>,
        diff_from_merge_base: Option<&TreeDiffStatus>,
    ) -> Option<FileStatus> {
        match (diff_from_head, diff_from_merge_base) {
            (None, None) => None,
            (Some(diff_from_head), None) => Some(diff_from_head),
            (Some(diff_from_head @ FileStatus::Unmerged(_)), _) => Some(diff_from_head),

            // file does not exist in HEAD
            // but *does* exist in work-tree
            // and *does* exist in merge-base
            (
                Some(FileStatus::Untracked)
                | Some(FileStatus::Tracked(TrackedStatus {
                    index_status: StatusCode::Added,
                    worktree_status: _,
                })),
                Some(_),
            ) => Some(FileStatus::Tracked(TrackedStatus {
                index_status: StatusCode::Modified,
                worktree_status: StatusCode::Modified,
            })),

            // file exists in HEAD
            // but *does not* exist in work-tree
            (Some(diff_from_head), Some(diff_from_merge_base)) if diff_from_head.is_deleted() => {
                match diff_from_merge_base {
                    TreeDiffStatus::Added => None, // unchanged, didn't exist in merge base or worktree
                    _ => Some(diff_from_head),
                }
            }

            // file exists in HEAD
            // and *does* exist in work-tree
            (Some(FileStatus::Tracked(_)), Some(tree_status)) => {
                Some(FileStatus::Tracked(TrackedStatus {
                    index_status: match tree_status {
                        TreeDiffStatus::Added { .. } => StatusCode::Added,
                        _ => StatusCode::Modified,
                    },
                    worktree_status: match tree_status {
                        TreeDiffStatus::Added => StatusCode::Added,
                        _ => StatusCode::Modified,
                    },
                }))
            }

            (_, Some(diff_from_merge_base)) => {
                Some(diff_status_to_file_status(diff_from_merge_base))
            }
        }
    }

    fn spawn_reload_tree_diff(&mut self, cx: &mut Context<Self>) {
        if !self.diff_base.is_merge_base() {
            return;
        }

        let task = cx.spawn(async move |this, cx| {
            Self::reload_tree_diff(this, cx).await.log_err();
        });

        self.tree_diff_base_task = Some(task);
        cx.notify();
    }

    pub fn is_tree_base_loading(&self) -> bool {
        self.tree_diff_base_task
            .as_ref()
            .is_some_and(|task| !task.is_ready())
    }

    pub async fn reload_tree_diff(this: WeakEntity<Self>, cx: &mut AsyncApp) -> Result<()> {
        let task = this.update(cx, |this, cx| {
            let DiffBase::Merge { base_ref, head_ref } = this.diff_base.clone() else {
                return None;
            };
            let Some(repo) = this.repo.as_ref() else {
                this.tree_diff.take();
                return None;
            };
            repo.update(cx, |repo, cx| {
                Some(repo.diff_tree(
                    DiffTreeType::MergeBase {
                        base: base_ref,
                        head: head_ref.unwrap_or_else(|| "HEAD".into()),
                    },
                    cx,
                ))
            })
        })?;
        let Some(task) = task else { return Ok(()) };

        let diff = task.await??;
        this.update(cx, |this, cx| {
            this.tree_diff = Some(diff);
            cx.emit(BranchDiffEvent::FileListChanged);
            cx.notify();
        })
    }

    pub fn repo(&self) -> Option<&Entity<Repository>> {
        self.repo.as_ref()
    }

    #[instrument(skip_all)]
    pub fn load_buffers(&mut self, cx: &mut Context<Self>) -> Vec<DiffBuffer> {
        let mut output = Vec::default();
        let Some(repo) = self.repo.clone() else {
            return output;
        };
        if self.diff_base.is_merge_base() && self.tree_diff.is_none() {
            return output;
        }

        let head_ref = match &self.diff_base {
            DiffBase::Merge { head_ref, .. } => head_ref.clone(),
            DiffBase::Head => None,
        };

        self.project.update(cx, |project, cx| {
            let work_dir = repo.read(cx).work_directory_abs_path.clone();
            let language_registry = project.languages().clone();
            let mut seen = HashSet::default();

            for item in repo.read(cx).cached_status() {
                seen.insert(item.repo_path.clone());
                let branch_diff = self
                    .tree_diff
                    .as_ref()
                    .and_then(|t| t.entries.get(&item.repo_path))
                    .cloned();
                if self.diff_base.has_explicit_head() && branch_diff.is_none() {
                    continue;
                }
                let Some(status) = self.merge_statuses(Some(item.status), branch_diff.as_ref())
                else {
                    continue;
                };
                if !status.has_changes() {
                    continue;
                }

                let Some(project_path) =
                    repo.read(cx).repo_path_to_project_path(&item.repo_path, cx)
                else {
                    continue;
                };
                let task = Self::load_buffer(
                    branch_diff,
                    project_path,
                    item.repo_path.clone(),
                    repo.clone(),
                    head_ref.clone(),
                    work_dir.clone(),
                    language_registry.clone(),
                    cx,
                );

                output.push(DiffBuffer {
                    repo_path: item.repo_path.clone(),
                    load: task,
                    file_status: item.status,
                });
            }
            let Some(tree_diff) = self.tree_diff.as_ref() else {
                return;
            };

            for (path, branch_diff) in tree_diff.entries.iter() {
                if seen.contains(&path) {
                    continue;
                }

                let Some(project_path) = repo.read(cx).repo_path_to_project_path(&path, cx) else {
                    continue;
                };
                let task = Self::load_buffer(
                    Some(branch_diff.clone()),
                    project_path,
                    path.clone(),
                    repo.clone(),
                    head_ref.clone(),
                    work_dir.clone(),
                    language_registry.clone(),
                    cx,
                );

                let file_status = diff_status_to_file_status(branch_diff);

                output.push(DiffBuffer {
                    repo_path: path.clone(),
                    load: task,
                    file_status,
                });
            }
        });
        output
    }

    #[instrument(skip_all)]
    fn load_buffer(
        branch_diff: Option<git::status::TreeDiffStatus>,
        project_path: crate::ProjectPath,
        repo_path: RepoPath,
        repo: Entity<Repository>,
        head_ref: Option<SharedString>,
        work_dir: Arc<std::path::Path>,
        language_registry: Arc<LanguageRegistry>,
        cx: &Context<'_, Project>,
    ) -> Task<Result<(Entity<Buffer>, Entity<BufferDiff>)>> {
        let worktree_id = project_path.worktree_id;
        let task = cx.spawn(async move |project, cx| {
            // PR / explicit-head mode: show the file at the head commit (read-only),
            // diffed against the base blob, without touching the working tree.
            if let Some(head_sha) = head_ref {
                let repo_path_str = repo_path.as_std_path().to_string_lossy().to_string();
                let output = smol::process::Command::new("git")
                    .current_dir(work_dir.as_ref())
                    .args(["show", &format!("{}:{}", head_sha, repo_path_str)])
                    .output()
                    .await?;
                let head_text = if output.status.success() {
                    String::from_utf8_lossy(&output.stdout).into_owned()
                } else {
                    String::new()
                };

                let (base_oid, is_deleted) = match &branch_diff {
                    Some(git::status::TreeDiffStatus::Modified { old }) => (Some(*old), false),
                    Some(git::status::TreeDiffStatus::Deleted { old }) => (Some(*old), true),
                    _ => (None, false),
                };
                let base_text = if let Some(oid) = base_oid {
                    repo.update(cx, |repo, cx| repo.load_blob_content(oid, cx))
                        .await
                        .ok()
                } else {
                    None
                };

                let display_name = repo_path_str
                    .rsplit('/')
                    .next()
                    .unwrap_or("")
                    .to_string();
                let file: Arc<dyn language::File> = Arc::new(GitBlobFile {
                    path: repo_path,
                    worktree_id,
                    is_deleted,
                    display_name,
                });
                let buffer = build_blob_buffer(head_text, file, &language_registry, cx).await?;
                let diff = build_blob_diff(base_text, &buffer, &language_registry, cx).await?;
                return Ok((buffer, diff));
            }

            let buffer = project
                .update(cx, |project, cx| project.open_buffer(project_path, cx))?
                .await?;

            let changes = if let Some(entry) = branch_diff {
                let oid = match entry {
                    git::status::TreeDiffStatus::Added { .. } => None,
                    git::status::TreeDiffStatus::Modified { old, .. }
                    | git::status::TreeDiffStatus::Deleted { old } => Some(old),
                };
                project
                    .update(cx, |project, cx| {
                        project.git_store().update(cx, |git_store, cx| {
                            git_store.open_diff_since(oid, buffer.clone(), repo, cx)
                        })
                    })?
                    .await?
            } else {
                project
                    .update(cx, |project, cx| {
                        project.open_uncommitted_diff(buffer.clone(), cx)
                    })?
                    .await?
            };
            Ok((buffer, changes))
        });
        task
    }
}

fn diff_status_to_file_status(branch_diff: &git::status::TreeDiffStatus) -> FileStatus {
    let file_status = match branch_diff {
        git::status::TreeDiffStatus::Added { .. } => FileStatus::Tracked(TrackedStatus {
            index_status: StatusCode::Added,
            worktree_status: StatusCode::Added,
        }),
        git::status::TreeDiffStatus::Modified { .. } => FileStatus::Tracked(TrackedStatus {
            index_status: StatusCode::Modified,
            worktree_status: StatusCode::Modified,
        }),
        git::status::TreeDiffStatus::Deleted { .. } => FileStatus::Tracked(TrackedStatus {
            index_status: StatusCode::Deleted,
            worktree_status: StatusCode::Deleted,
        }),
    };
    file_status
}

#[derive(Debug)]
pub struct DiffBuffer {
    pub repo_path: RepoPath,
    pub file_status: FileStatus,
    pub load: Task<Result<(Entity<Buffer>, Entity<BufferDiff>)>>,
}

/// A synthetic `File` for a read-only buffer backed by a git blob (a file at a
/// specific commit), used when reviewing a PR head that isn't checked out.
struct GitBlobFile {
    path: RepoPath,
    worktree_id: WorktreeId,
    is_deleted: bool,
    display_name: String,
}

impl language::File for GitBlobFile {
    fn as_local(&self) -> Option<&dyn language::LocalFile> {
        None
    }

    fn disk_state(&self) -> DiskState {
        DiskState::Historic {
            was_deleted: self.is_deleted,
        }
    }

    fn path_style(&self, _: &App) -> PathStyle {
        PathStyle::local()
    }

    fn path(&self) -> &Arc<RelPath> {
        self.path.as_ref()
    }

    fn full_path(&self, _: &App) -> PathBuf {
        self.path.as_std_path().to_path_buf()
    }

    fn file_name<'a>(&'a self, _: &'a App) -> &'a str {
        self.display_name.as_ref()
    }

    fn worktree_id(&self, _: &App) -> WorktreeId {
        self.worktree_id
    }

    fn to_proto(&self, _cx: &App) -> language::proto::File {
        unimplemented!()
    }

    fn is_private(&self) -> bool {
        false
    }

    fn can_open(&self) -> bool {
        true
    }
}

/// Build a read-only buffer holding `text` (a git blob's content), with a
/// synthetic `File` so syntax highlighting and the path resolve.
async fn build_blob_buffer(
    mut text: String,
    file: Arc<dyn language::File>,
    language_registry: &Arc<LanguageRegistry>,
    cx: &mut AsyncApp,
) -> Result<Entity<Buffer>> {
    let line_ending = LineEnding::detect(&text);
    LineEnding::normalize(&mut text);
    let text = Rope::from(text);
    let language = cx.update(|cx| language_registry.language_for_file(&file, Some(&text), cx));
    let language = if let Some(language) = language {
        language_registry
            .load_language(&language)
            .await
            .ok()
            .and_then(|e| e.log_err())
    } else {
        None
    };
    let buffer = cx.new(|cx| {
        let buffer = TextBuffer::new_normalized(
            ReplicaId::LOCAL,
            cx.entity_id().as_non_zero_u64().into(),
            line_ending,
            text,
        );
        let mut buffer = Buffer::build(buffer, Some(file), Capability::ReadOnly);
        buffer.set_language_async(language, cx);
        buffer
    });
    Ok(buffer)
}

/// Build a `BufferDiff` of `buffer` against `old_text` (the base blob).
async fn build_blob_diff(
    mut old_text: Option<String>,
    buffer: &Entity<Buffer>,
    language_registry: &Arc<LanguageRegistry>,
    cx: &mut AsyncApp,
) -> Result<Entity<BufferDiff>> {
    if let Some(old_text) = &mut old_text {
        LineEnding::normalize(old_text);
    }

    let language = cx.update(|cx| buffer.read(cx).language().cloned());
    let buffer = cx.update(|cx| buffer.read(cx).snapshot());

    let diff = cx.new(|cx| BufferDiff::new(&buffer.text, cx));

    let update = diff
        .update(cx, |diff, cx| {
            diff.update_diff(
                buffer.text.clone(),
                old_text.map(|old_text| Arc::from(old_text.as_str())),
                Some(true),
                language.clone(),
                cx,
            )
        })
        .await;

    diff.update(cx, |diff, cx| {
        diff.language_changed(language, Some(language_registry.clone()), cx);
        diff.set_snapshot(update, &buffer.text, cx)
    })
    .await;

    Ok(diff)
}
