use gpui::SharedString;
use std::future::Future;
use std::pin::Pin;

#[derive(Clone, Debug, PartialEq)]
pub enum PullRequestState {
    Open,
    Closed,
    All,
}

#[derive(Clone, Debug)]
pub enum ReviewStatus {
    Pending,
    Approved,
    ChangesRequested,
    Commented,
}

#[derive(Clone, Debug)]
pub enum FileChangeStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
}

/// One page of pull requests plus the cursor needed to fetch the next page and
/// the repository-wide total for the current filter.
#[derive(Clone, Debug, Default)]
pub struct PullRequestPage {
    pub pull_requests: Vec<PullRequestInfo>,
    pub total_count: usize,
    pub end_cursor: Option<String>,
    pub has_next_page: bool,
}

#[derive(Clone, Debug)]
pub struct PullRequestFile {
    pub path: SharedString,
    pub status: FileChangeStatus,
    pub additions: u32,
    pub deletions: u32,
}

#[derive(Clone, Debug)]
pub struct ReviewComment {
    pub id: u64,
    /// GraphQL global node id of this comment, needed for reaction mutations.
    /// Empty until reactions are merged in (e.g. for a freshly-posted comment).
    pub node_id: SharedString,
    pub author: SharedString,
    pub body: SharedString,
    pub created_at: SharedString,
    pub path: Option<SharedString>,
    pub line: Option<u32>,
    /// First line of a multi-line comment range (None for single-line comments).
    pub start_line: Option<u32>,
    pub reply_to: Option<u64>,
    pub diff_hunk: Option<SharedString>,
    /// Emoji reaction tallies for this comment (only contents with count > 0 are
    /// worth displaying; the full set is kept so the picker knows current state).
    pub reactions: Vec<ReactionGroup>,
}

/// The eight reaction contents GitHub supports, in display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReactionContent {
    ThumbsUp,
    ThumbsDown,
    Laugh,
    Hooray,
    Confused,
    Heart,
    Rocket,
    Eyes,
}

impl ReactionContent {
    pub const ALL: [ReactionContent; 8] = [
        ReactionContent::ThumbsUp,
        ReactionContent::ThumbsDown,
        ReactionContent::Laugh,
        ReactionContent::Hooray,
        ReactionContent::Confused,
        ReactionContent::Heart,
        ReactionContent::Rocket,
        ReactionContent::Eyes,
    ];

    /// The GraphQL `ReactionContent` enum value (also used in mutation inputs).
    pub fn graphql(self) -> &'static str {
        match self {
            ReactionContent::ThumbsUp => "THUMBS_UP",
            ReactionContent::ThumbsDown => "THUMBS_DOWN",
            ReactionContent::Laugh => "LAUGH",
            ReactionContent::Hooray => "HOORAY",
            ReactionContent::Confused => "CONFUSED",
            ReactionContent::Heart => "HEART",
            ReactionContent::Rocket => "ROCKET",
            ReactionContent::Eyes => "EYES",
        }
    }

    pub fn from_graphql(value: &str) -> Option<Self> {
        Some(match value {
            "THUMBS_UP" => ReactionContent::ThumbsUp,
            "THUMBS_DOWN" => ReactionContent::ThumbsDown,
            "LAUGH" => ReactionContent::Laugh,
            "HOORAY" => ReactionContent::Hooray,
            "CONFUSED" => ReactionContent::Confused,
            "HEART" => ReactionContent::Heart,
            "ROCKET" => ReactionContent::Rocket,
            "EYES" => ReactionContent::Eyes,
            _ => return None,
        })
    }

    pub fn emoji(self) -> &'static str {
        match self {
            ReactionContent::ThumbsUp => "👍",
            ReactionContent::ThumbsDown => "👎",
            ReactionContent::Laugh => "😄",
            ReactionContent::Hooray => "🎉",
            ReactionContent::Confused => "😕",
            ReactionContent::Heart => "❤️",
            ReactionContent::Rocket => "🚀",
            ReactionContent::Eyes => "👀",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ReactionContent::ThumbsUp => "Thumbs up",
            ReactionContent::ThumbsDown => "Thumbs down",
            ReactionContent::Laugh => "Laugh",
            ReactionContent::Hooray => "Hooray",
            ReactionContent::Confused => "Confused",
            ReactionContent::Heart => "Heart",
            ReactionContent::Rocket => "Rocket",
            ReactionContent::Eyes => "Eyes",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ReactionGroup {
    pub content: ReactionContent,
    pub count: u32,
    pub viewer_reacted: bool,
}

/// Reaction state for a single comment, keyed back to the REST comment by its
/// numeric `database_id` (matches `ReviewComment::id`).
#[derive(Clone, Debug)]
pub struct CommentReactions {
    pub database_id: u64,
    pub node_id: SharedString,
    pub reactions: Vec<ReactionGroup>,
}

/// Aggregate CI rollup state for a PR's head commit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CheckRollup {
    Success,
    Failure,
    Pending,
}

#[derive(Clone, Debug)]
pub struct PrLabel {
    pub name: SharedString,
    /// 6-digit hex color (no leading '#'), as GitHub returns it.
    pub color: SharedString,
}

/// Mergeability, CI rollup, and labels for a PR — fetched together on selection.
#[derive(Clone, Debug, Default)]
pub struct PullRequestStatus {
    pub mergeable: Option<bool>,
    pub checks: Option<CheckRollup>,
    pub labels: Vec<PrLabel>,
}

/// Full PR data fetched on demand. Only `info` is currently consumed (when
/// restoring a review); the fetch is kept coarse for future use.
#[derive(Clone, Debug)]
pub struct PullRequestDetails {
    pub info: PullRequestInfo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}

impl MergeMethod {
    /// Label for the merge-method dropdown entries.
    pub fn label(self) -> &'static str {
        match self {
            MergeMethod::Merge => "Create a merge commit",
            MergeMethod::Squash => "Squash and merge",
            MergeMethod::Rebase => "Rebase and merge",
        }
    }

    /// Label for the merge action button, reflecting the selected strategy.
    pub fn button_label(self) -> &'static str {
        match self {
            MergeMethod::Merge => "Merge commit",
            MergeMethod::Squash => "Squash and merge",
            MergeMethod::Rebase => "Rebase and merge",
        }
    }
}

#[derive(Clone, Debug)]
pub struct PullRequestInfo {
    pub number: u32,
    /// GraphQL global node id (needed for viewed-file mutations).
    pub node_id: SharedString,
    pub title: SharedString,
    pub author: SharedString,
    pub base_ref: SharedString,
    pub head_ref: SharedString,
    pub base_sha: SharedString,
    pub head_sha: SharedString,
    pub created_at: SharedString,
    pub updated_at: SharedString,
    pub review_status: ReviewStatus,
    pub is_draft: bool,
    pub mergeable: Option<bool>,
    pub checks: Option<CheckRollup>,
    pub labels: Vec<PrLabel>,
    /// Number of distinct approving reviews so far.
    pub approvals: u32,
    /// Approvals required by branch protection, if readable (None when there's
    /// no protection rule or the token can't read it).
    pub required_approvals: Option<u32>,
    /// Combined conversation comments + inline review threads, for a list-row
    /// activity indicator.
    pub comment_count: u32,
    /// Distinct logins of everyone who reviewed or commented (for a facepile).
    pub participants: Vec<SharedString>,
}

pub trait ReviewProvider: Send + Sync {
    /// Fetch one page of pull requests. `after` is the opaque cursor returned by
    /// a previous page (None for the first page).
    fn fetch_pull_requests(
        &self,
        owner: &str,
        repo: &str,
        state: PullRequestState,
        after: Option<String>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<PullRequestPage>> + Send>>;

    fn fetch_pull_request_details(
        &self,
        owner: &str,
        repo: &str,
        number: u32,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<PullRequestDetails>> + Send>>;

    /// Fetch just the PR description (markdown body), which the list query omits.
    fn fetch_pull_request_body(
        &self,
        owner: &str,
        repo: &str,
        number: u32,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send>>;

    /// Fetch mergeability, CI check rollup, and labels in one request.
    fn fetch_pull_request_status(
        &self,
        owner: &str,
        repo: &str,
        number: u32,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<PullRequestStatus>> + Send>>;

    fn fetch_pull_request_files(
        &self,
        owner: &str,
        repo: &str,
        number: u32,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Vec<PullRequestFile>>> + Send>>;

    fn fetch_reviews(
        &self,
        owner: &str,
        repo: &str,
        number: u32,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Vec<ReviewComment>>> + Send>>;

    fn submit_comment(
        &self,
        owner: &str,
        repo: &str,
        number: u32,
        body: &str,
        path: Option<&str>,
        line: Option<u32>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<ReviewComment>> + Send>>;

    /// Reply to an existing review comment thread. `in_reply_to_id` is the id of
    /// the comment being replied to.
    fn reply_to_comment(
        &self,
        owner: &str,
        repo: &str,
        number: u32,
        body: &str,
        in_reply_to_id: u64,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<ReviewComment>> + Send>>;

    /// Post a new inline review comment on a diff line (or line range, when
    /// `start_line` is set). `commit_id` is the SHA the comment is anchored to,
    /// and `side` is "RIGHT" (head) or "LEFT" (base).
    fn submit_inline_comment(
        &self,
        owner: &str,
        repo: &str,
        number: u32,
        body: &str,
        commit_id: &str,
        path: &str,
        start_line: Option<u32>,
        line: u32,
        side: &str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<ReviewComment>> + Send>>;

    fn submit_review(
        &self,
        owner: &str,
        repo: &str,
        number: u32,
        status: ReviewStatus,
        body: Option<&str>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>;

    /// Fetch the reaction tallies for every comment on a PR, keyed by the
    /// comment's numeric `database_id`.
    fn fetch_comment_reactions(
        &self,
        _owner: &str,
        _repo: &str,
        _number: u32,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Vec<CommentReactions>>> + Send>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    /// Add or remove the current user's reaction of `content` on the comment
    /// identified by its GraphQL node id.
    fn set_reaction(
        &self,
        _comment_node_id: &str,
        _content: ReactionContent,
        _add: bool,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> {
        Box::pin(async { Err(anyhow::anyhow!("reactions not supported by this provider")) })
    }

    /// Paths the current user has marked as viewed on this PR (by GraphQL node id).
    fn fetch_viewed_files(
        &self,
        pr_node_id: &str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Vec<String>>> + Send>>;

    /// Merge the pull request using the given method.
    fn merge_pull_request(
        &self,
        _owner: &str,
        _repo: &str,
        _number: u32,
        _merge_method: MergeMethod,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> {
        Box::pin(async { Err(anyhow::anyhow!("merge not supported by this provider")) })
    }

    /// Mark/unmark a file as viewed on the PR identified by its GraphQL node id.
    fn mark_file_viewed(
        &self,
        pr_node_id: &str,
        path: &str,
        viewed: bool,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>;

}
