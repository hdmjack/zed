use crate::review_provider::*;
use anyhow::{Context as _, bail};
use futures::AsyncReadExt;
use gpui::SharedString;
use http_client::{AsyncBody, HttpClient, HttpRequestExt, RedirectPolicy, Request};
use serde::Deserialize;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

const GITHUB_API_URL: &str = "https://api.github.com";

#[derive(Deserialize)]
struct GhPullRequest {
    number: u32,
    title: String,
    user: GhUser,
    body: Option<String>,
    state: String,
    #[serde(default)]
    draft: bool,
    base: GhRef,
    head: GhRef,
    created_at: String,
    updated_at: String,
}

#[derive(Deserialize)]
struct GhUser {
    login: String,
}

#[derive(Deserialize)]
struct GhRef {
    #[serde(rename = "ref")]
    ref_name: String,
    sha: String,
}

#[derive(Deserialize)]
struct GhFile {
    filename: String,
    status: String,
    additions: u32,
    deletions: u32,
    previous_filename: Option<String>,
}

#[derive(Deserialize)]
struct GhReviewComment {
    id: u64,
    user: GhUser,
    body: String,
    created_at: String,
    path: Option<String>,
    line: Option<u32>,
    in_reply_to_id: Option<u64>,
    diff_hunk: Option<String>,
}

#[derive(Deserialize)]
struct GhReview {
    id: u64,
    user: GhUser,
    body: Option<String>,
    state: String,
    submitted_at: Option<String>,
}

#[derive(Deserialize)]
struct GhIssueComment {
    id: u64,
    user: GhUser,
    body: String,
    created_at: String,
}

async fn github_get<T: serde::de::DeserializeOwned>(
    http_client: &Arc<dyn HttpClient>,
    token: &Option<String>,
    url: &str,
) -> anyhow::Result<T> {
    let mut builder = Request::get(url)
        .header("Accept", "application/vnd.github.v3+json")
        .follow_redirects(RedirectPolicy::FollowAll);

    if let Some(token) = token {
        builder = builder.header("Authorization", format!("Bearer {}", token));
    }

    let request = builder.body(AsyncBody::default())?;
    let mut response = http_client.send(request).await?;

    let mut body = Vec::new();
    response.body_mut().read_to_end(&mut body).await?;

    if !response.status().is_success() {
        let text = String::from_utf8_lossy(&body);
        bail!("GitHub API error {}: {}", response.status().as_u16(), text);
    }

    serde_json::from_slice(&body).context("failed to parse GitHub response")
}

async fn github_post<T: serde::de::DeserializeOwned>(
    http_client: &Arc<dyn HttpClient>,
    token: &Option<String>,
    url: &str,
    json_body: String,
) -> anyhow::Result<T> {
    let mut builder = Request::post(url)
        .header("Accept", "application/vnd.github.v3+json")
        .header("Content-Type", "application/json")
        .follow_redirects(RedirectPolicy::FollowAll);

    if let Some(token) = token {
        builder = builder.header("Authorization", format!("Bearer {}", token));
    }

    let request = builder.body(AsyncBody::from(json_body))?;
    let mut response = http_client.send(request).await?;

    let mut body = Vec::new();
    response.body_mut().read_to_end(&mut body).await?;

    if !response.status().is_success() {
        let text = String::from_utf8_lossy(&body);
        bail!("GitHub API error {}: {}", response.status().as_u16(), text);
    }

    serde_json::from_slice(&body).context("failed to parse GitHub response")
}

fn map_pr_state(state: &str) -> PullRequestState {
    match state {
        "open" => PullRequestState::Open,
        "closed" => PullRequestState::Closed,
        _ => PullRequestState::Closed,
    }
}

fn map_file_status(status: &str, previous_filename: Option<String>) -> FileChangeStatus {
    match status {
        "added" => FileChangeStatus::Added,
        "modified" | "changed" => FileChangeStatus::Modified,
        "removed" => FileChangeStatus::Deleted,
        "renamed" => FileChangeStatus::Renamed {
            from: previous_filename.unwrap_or_default().into(),
        },
        _ => FileChangeStatus::Modified,
    }
}

fn map_pull_request(pr: GhPullRequest) -> PullRequestInfo {
    PullRequestInfo {
        number: pr.number,
        title: pr.title.into(),
        author: pr.user.login.into(),
        description: pr.body.unwrap_or_default().into(),
        state: map_pr_state(&pr.state),
        base_ref: pr.base.ref_name.into(),
        head_ref: pr.head.ref_name.into(),
        base_sha: pr.base.sha.into(),
        head_sha: pr.head.sha.into(),
        created_at: pr.created_at.into(),
        updated_at: pr.updated_at.into(),
        review_status: ReviewStatus::Pending,
        is_draft: pr.draft,
    }
}

fn map_file(file: GhFile) -> PullRequestFile {
    PullRequestFile {
        path: file.filename.into(),
        status: map_file_status(&file.status, file.previous_filename),
        additions: file.additions,
        deletions: file.deletions,
    }
}

fn map_review_comment(comment: GhReviewComment) -> ReviewComment {
    ReviewComment {
        id: comment.id,
        author: comment.user.login.into(),
        body: comment.body.into(),
        created_at: comment.created_at.into(),
        path: comment.path.map(SharedString::from),
        line: comment.line,
        reply_to: comment.in_reply_to_id,
        diff_hunk: comment.diff_hunk.map(SharedString::from),
    }
}

// The PR list is fetched via GraphQL so we download only the handful of fields
// the list renders, instead of REST's full PR objects (each of which embeds the
// entire head/base repository objects, links, labels, and body).
#[derive(Deserialize)]
struct GraphQlResponse {
    data: Option<PrQueryData>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

#[derive(Deserialize)]
struct GraphQlError {
    message: String,
}

#[derive(Deserialize)]
struct PrQueryData {
    repository: Option<RepositoryPrs>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepositoryPrs {
    pull_requests: PrConnection,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrConnection {
    total_count: usize,
    page_info: GqlPageInfo,
    nodes: Vec<GqlPullRequest>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlPageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlPullRequest {
    number: u32,
    title: String,
    state: String,
    created_at: String,
    updated_at: String,
    base_ref_name: String,
    head_ref_name: String,
    base_ref_oid: String,
    head_ref_oid: String,
    author: Option<GqlAuthor>,
    review_decision: Option<String>,
    is_draft: bool,
}

#[derive(Deserialize)]
struct GqlAuthor {
    login: String,
}

fn map_graphql_state(state: &str) -> PullRequestState {
    match state {
        "OPEN" => PullRequestState::Open,
        "MERGED" => PullRequestState::Merged,
        _ => PullRequestState::Closed,
    }
}

fn map_review_decision(decision: Option<&str>) -> ReviewStatus {
    match decision {
        Some("APPROVED") => ReviewStatus::Approved,
        Some("CHANGES_REQUESTED") => ReviewStatus::ChangesRequested,
        _ => ReviewStatus::Pending,
    }
}

fn map_graphql_pr(pr: GqlPullRequest) -> PullRequestInfo {
    PullRequestInfo {
        number: pr.number,
        title: pr.title.into(),
        author: pr.author.map(|a| a.login).unwrap_or_default().into(),
        // The list doesn't render the body; fetch it lazily with PR details.
        description: SharedString::default(),
        state: map_graphql_state(&pr.state),
        base_ref: pr.base_ref_name.into(),
        head_ref: pr.head_ref_name.into(),
        base_sha: pr.base_ref_oid.into(),
        head_sha: pr.head_ref_oid.into(),
        created_at: pr.created_at.into(),
        updated_at: pr.updated_at.into(),
        review_status: map_review_decision(pr.review_decision.as_deref()),
        is_draft: pr.is_draft,
    }
}

pub struct GitHubProvider {
    http_client: Arc<dyn HttpClient>,
    token: Option<String>,
}

impl GitHubProvider {
    pub fn new(http_client: Arc<dyn HttpClient>, token: Option<String>) -> Self {
        Self { http_client, token }
    }
}

impl ReviewProvider for GitHubProvider {
    fn name(&self) -> &'static str {
        "GitHub"
    }

    fn fetch_pull_requests(
        &self,
        owner: &str,
        repo: &str,
        state: PullRequestState,
        after: Option<String>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<PullRequestPage>> + Send>> {
        // A trailing comma is included so the clause drops cleanly into the
        // argument list before `orderBy`; "All" omits the filter entirely.
        let states_clause = match &state {
            PullRequestState::Open => "states: [OPEN], ",
            PullRequestState::Closed => "states: [CLOSED, MERGED], ",
            PullRequestState::Merged => "states: [MERGED], ",
            PullRequestState::All => "",
        };
        let query = format!(
            "query($owner: String!, $repo: String!, $first: Int!, $after: String) {{ \
               repository(owner: $owner, name: $repo) {{ \
                 pullRequests(first: $first, after: $after, {states_clause}orderBy: {{field: UPDATED_AT, direction: DESC}}) {{ \
                   totalCount \
                   pageInfo {{ hasNextPage endCursor }} \
                   nodes {{ \
                     number title state createdAt updatedAt isDraft \
                     baseRefName headRefName baseRefOid headRefOid \
                     author {{ login }} reviewDecision \
                   }} \
                 }} \
               }} \
             }}"
        );
        let owner = owner.to_string();
        let repo = repo.to_string();
        let url = format!("{GITHUB_API_URL}/graphql");
        let http_client = self.http_client.clone();
        let token = self.token.clone();

        Box::pin(async move {
            let body = serde_json::json!({
                "query": query,
                "variables": { "owner": owner, "repo": repo, "first": 30, "after": after },
            })
            .to_string();
            let response: GraphQlResponse = github_post(&http_client, &token, &url, body).await?;
            if !response.errors.is_empty() {
                let message = response
                    .errors
                    .into_iter()
                    .map(|error| error.message)
                    .collect::<Vec<_>>()
                    .join("; ");
                bail!("GitHub GraphQL error: {message}");
            }
            let connection = response
                .data
                .and_then(|data| data.repository)
                .map(|repository| repository.pull_requests);
            let Some(connection) = connection else {
                return Ok(PullRequestPage::default());
            };
            Ok(PullRequestPage {
                pull_requests: connection.nodes.into_iter().map(map_graphql_pr).collect(),
                total_count: connection.total_count,
                end_cursor: connection.page_info.end_cursor,
                has_next_page: connection.page_info.has_next_page,
            })
        })
    }

    fn fetch_pull_request_details(
        &self,
        owner: &str,
        repo: &str,
        number: u32,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<PullRequestDetails>> + Send>> {
        let pr_url = format!("{GITHUB_API_URL}/repos/{owner}/{repo}/pulls/{number}");
        let files_url =
            format!("{GITHUB_API_URL}/repos/{owner}/{repo}/pulls/{number}/files?per_page=100");
        let http_client = self.http_client.clone();
        let token = self.token.clone();

        Box::pin(async move {
            let gh_pr: GhPullRequest = github_get(&http_client, &token, &pr_url).await?;
            let gh_files: Vec<GhFile> = github_get(&http_client, &token, &files_url).await?;

            Ok(PullRequestDetails {
                info: map_pull_request(gh_pr),
                files: gh_files.into_iter().map(map_file).collect(),
                comments: Vec::new(),
                checks: Vec::new(),
                mergeable: None,
                labels: Vec::new(),
            })
        })
    }

    fn fetch_pull_request_files(
        &self,
        owner: &str,
        repo: &str,
        number: u32,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Vec<PullRequestFile>>> + Send>> {
        let url =
            format!("{GITHUB_API_URL}/repos/{owner}/{repo}/pulls/{number}/files?per_page=100");
        let http_client = self.http_client.clone();
        let token = self.token.clone();

        Box::pin(async move {
            let gh_files: Vec<GhFile> = github_get(&http_client, &token, &url).await?;
            Ok(gh_files.into_iter().map(map_file).collect())
        })
    }

    fn fetch_reviews(
        &self,
        owner: &str,
        repo: &str,
        number: u32,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Vec<ReviewComment>>> + Send>> {
        let comments_url =
            format!("{GITHUB_API_URL}/repos/{owner}/{repo}/pulls/{number}/comments?per_page=100");
        let reviews_url =
            format!("{GITHUB_API_URL}/repos/{owner}/{repo}/pulls/{number}/reviews?per_page=100");
        let http_client = self.http_client.clone();
        let token = self.token.clone();

        Box::pin(async move {
            // Fetch inline code comments
            let gh_comments: Vec<GhReviewComment> =
                github_get(&http_client, &token, &comments_url).await?;

            // Fetch top-level review submissions (approve, request changes, etc.)
            let gh_reviews: Vec<GhReview> = github_get(&http_client, &token, &reviews_url).await?;

            let mut comments: Vec<ReviewComment> =
                gh_comments.into_iter().map(map_review_comment).collect();

            // Add review-level comments (non-empty body only)
            for review in gh_reviews {
                if let Some(body) = review.body {
                    if !body.is_empty() {
                        comments.push(ReviewComment {
                            id: review.id,
                            author: review.user.login.into(),
                            body: body.into(),
                            created_at: review.submitted_at.unwrap_or_default().into(),
                            path: None,
                            line: None,
                            reply_to: None,
                            diff_hunk: None,
                        });
                    }
                }
            }

            Ok(comments)
        })
    }

    fn submit_comment(
        &self,
        owner: &str,
        repo: &str,
        number: u32,
        body: &str,
        path: Option<&str>,
        line: Option<u32>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<ReviewComment>> + Send>> {
        // General PR comments use the issues API
        let url = format!("{GITHUB_API_URL}/repos/{owner}/{repo}/issues/{number}/comments");
        let json = serde_json::json!({ "body": body }).to_string();
        let http_client = self.http_client.clone();
        let token = self.token.clone();
        let path = path.map(|p| SharedString::from(p.to_string()));

        Box::pin(async move {
            let gh_comment: GhIssueComment = github_post(&http_client, &token, &url, json).await?;
            Ok(ReviewComment {
                id: gh_comment.id,
                author: gh_comment.user.login.into(),
                body: gh_comment.body.into(),
                created_at: gh_comment.created_at.into(),
                path,
                line,
                reply_to: None,
                diff_hunk: None,
            })
        })
    }

    fn reply_to_comment(
        &self,
        owner: &str,
        repo: &str,
        number: u32,
        body: &str,
        in_reply_to_id: u64,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<ReviewComment>> + Send>> {
        let url = format!(
            "{GITHUB_API_URL}/repos/{owner}/{repo}/pulls/{number}/comments/{in_reply_to_id}/replies"
        );
        let json = serde_json::json!({ "body": body }).to_string();
        let http_client = self.http_client.clone();
        let token = self.token.clone();

        Box::pin(async move {
            let gh_comment: GhReviewComment =
                github_post(&http_client, &token, &url, json).await?;
            Ok(map_review_comment(gh_comment))
        })
    }

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
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<ReviewComment>> + Send>> {
        let url = format!("{GITHUB_API_URL}/repos/{owner}/{repo}/pulls/{number}/comments");
        let mut payload = serde_json::json!({
            "body": body,
            "commit_id": commit_id,
            "path": path,
            "line": line,
            "side": side,
        });
        if let Some(start_line) = start_line {
            payload["start_line"] = serde_json::json!(start_line);
            payload["start_side"] = serde_json::json!(side);
        }
        let json = payload.to_string();
        let http_client = self.http_client.clone();
        let token = self.token.clone();

        Box::pin(async move {
            let gh_comment: GhReviewComment =
                github_post(&http_client, &token, &url, json).await?;
            Ok(map_review_comment(gh_comment))
        })
    }

    fn submit_review(
        &self,
        owner: &str,
        repo: &str,
        number: u32,
        status: ReviewStatus,
        body: Option<&str>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> {
        let event = match status {
            ReviewStatus::Approved => "APPROVE",
            ReviewStatus::ChangesRequested => "REQUEST_CHANGES",
            ReviewStatus::Commented => "COMMENT",
            ReviewStatus::Pending => "PENDING",
        };
        let url = format!("{GITHUB_API_URL}/repos/{owner}/{repo}/pulls/{number}/reviews");
        let json = serde_json::json!({
            "event": event,
            "body": body.unwrap_or(""),
        })
        .to_string();
        let http_client = self.http_client.clone();
        let token = self.token.clone();

        Box::pin(async move {
            let _: serde_json::Value = github_post(&http_client, &token, &url, json).await?;
            Ok(())
        })
    }

    fn merge_pull_request(
        &self,
        owner: &str,
        repo: &str,
        number: u32,
        merge_method: MergeMethod,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> {
        let method = match merge_method {
            MergeMethod::Merge => "merge",
            MergeMethod::Squash => "squash",
            MergeMethod::Rebase => "rebase",
        };
        let url = format!("{GITHUB_API_URL}/repos/{owner}/{repo}/pulls/{number}/merge");
        let json = serde_json::json!({ "merge_method": method }).to_string();
        let http_client = self.http_client.clone();
        let token = self.token.clone();

        Box::pin(async move {
            let mut builder = http_client::Request::builder()
                .method(http_client::Method::PUT)
                .uri(&url)
                .header("Accept", "application/vnd.github.v3+json")
                .header("Content-Type", "application/json")
                .follow_redirects(RedirectPolicy::FollowAll);

            if let Some(token) = &token {
                builder = builder.header("Authorization", format!("Bearer {}", token));
            }

            let request = builder.body(AsyncBody::from(json))?;
            let mut response = http_client.send(request).await?;

            let mut body = Vec::new();
            response.body_mut().read_to_end(&mut body).await?;

            if !response.status().is_success() {
                let text = String::from_utf8_lossy(&body);
                bail!(
                    "GitHub merge error {}: {}",
                    response.status().as_u16(),
                    text
                );
            }

            Ok(())
        })
    }
}
