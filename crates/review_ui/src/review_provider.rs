use gpui::SharedString;
use std::future::Future;
use std::pin::Pin;

#[derive(Clone, Debug, PartialEq)]
pub enum PullRequestState {
    Open,
    Closed,
    Merged,
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
    Renamed { from: SharedString },
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
    pub author: SharedString,
    pub body: SharedString,
    pub created_at: SharedString,
    pub path: Option<SharedString>,
    pub line: Option<u32>,
    pub reply_to: Option<u64>,
    pub diff_hunk: Option<SharedString>,
}

#[derive(Clone, Debug)]
pub enum CheckStatus {
    Pending,
    Success,
    Failure,
    Cancelled,
}

#[derive(Clone, Debug)]
pub struct CheckRun {
    pub name: SharedString,
    pub status: CheckStatus,
    pub url: Option<SharedString>,
    pub started_at: Option<SharedString>,
    pub completed_at: Option<SharedString>,
}

#[derive(Clone, Debug)]
pub struct PullRequestDetails {
    pub info: PullRequestInfo,
    pub files: Vec<PullRequestFile>,
    pub comments: Vec<ReviewComment>,
    pub checks: Vec<CheckRun>,
    pub mergeable: Option<bool>,
    pub labels: Vec<SharedString>,
}

#[derive(Clone, Debug)]
pub enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}

#[derive(Clone, Debug)]
pub struct PullRequestInfo {
    pub number: u32,
    pub title: SharedString,
    pub author: SharedString,
    pub description: SharedString,
    pub state: PullRequestState,
    pub base_ref: SharedString,
    pub head_ref: SharedString,
    pub base_sha: SharedString,
    pub head_sha: SharedString,
    pub created_at: SharedString,
    pub updated_at: SharedString,
    pub review_status: ReviewStatus,
    pub is_draft: bool,
}

pub trait ReviewProvider: Send + Sync {
    fn name(&self) -> &'static str;
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

    fn react_to_comment(
        &self,
        _owner: &str,
        _repo: &str,
        _comment_id: u64,
        _reaction: &str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> {
        Box::pin(async { Err(anyhow::anyhow!("reactions not supported by this provider")) })
    }

    fn merge_pull_request(
        &self,
        _owner: &str,
        _repo: &str,
        _number: u32,
        _merge_method: MergeMethod,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> {
        Box::pin(async { Err(anyhow::anyhow!("merge not supported by this provider")) })
    }

    fn mark_file_viewed(
        &self,
        _owner: &str,
        _repo: &str,
        _number: u32,
        _path: &str,
        _viewed: bool,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> {
        Box::pin(async {
            Err(anyhow::anyhow!(
                "mark file viewed not supported by this provider"
            ))
        })
    }

    fn request_reviewers(
        &self,
        _owner: &str,
        _repo: &str,
        _number: u32,
        _reviewers: &[&str],
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> {
        Box::pin(async {
            Err(anyhow::anyhow!(
                "request reviewers not supported by this provider"
            ))
        })
    }

    fn fetch_checks(
        &self,
        _owner: &str,
        _repo: &str,
        _number: u32,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Vec<CheckRun>>> + Send>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn fetch_mergeable(
        &self,
        _owner: &str,
        _repo: &str,
        _number: u32,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Option<bool>>> + Send>> {
        Box::pin(async { Ok(None) })
    }

    fn fetch_labels(
        &self,
        _owner: &str,
        _repo: &str,
        _number: u32,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Vec<SharedString>>> + Send>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn apply_suggestion(
        &self,
        _owner: &str,
        _repo: &str,
        _number: u32,
        _comment_id: u64,
        _suggested_code: &str,
        _path: &str,
        _line: u32,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> {
        Box::pin(async {
            Err(anyhow::anyhow!(
                "apply suggestion not supported by this provider"
            ))
        })
    }
}
