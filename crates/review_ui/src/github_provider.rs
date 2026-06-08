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
    node_id: String,
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
    start_line: Option<u32>,
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
        node_id: pr.node_id.into(),
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
        mergeable: None,
        checks: None,
        labels: Vec::new(),
        approvals: 0,
        required_approvals: None,
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
        start_line: comment.start_line,
        reply_to: comment.in_reply_to_id,
        diff_hunk: comment.diff_hunk.map(SharedString::from),
        node_id: SharedString::default(),
        reactions: Vec::new(),
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
    id: String,
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
    mergeable: Option<String>,
    labels: Option<GqlLabels>,
    commits: Option<GqlCommits>,
    latest_opinionated_reviews: Option<GqlReviewNodes>,
    base_ref: Option<GqlBaseRef>,
}

#[derive(Deserialize)]
struct GqlReviewNodes {
    nodes: Vec<GqlReviewState>,
}

#[derive(Deserialize)]
struct GqlReviewState {
    state: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlBaseRef {
    branch_protection_rule: Option<GqlBranchProtection>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlBranchProtection {
    required_approving_review_count: Option<u32>,
}

#[derive(Deserialize)]
struct GqlAuthor {
    login: String,
}

#[derive(Deserialize)]
struct PrStatusResponse {
    data: Option<PrStatusData>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

#[derive(Deserialize)]
struct PrStatusData {
    repository: Option<PrStatusRepo>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrStatusRepo {
    pull_request: Option<GqlPrStatus>,
}

#[derive(Deserialize)]
struct GqlPrStatus {
    mergeable: Option<String>,
    labels: Option<GqlLabels>,
    commits: Option<GqlCommits>,
}

#[derive(Deserialize)]
struct GqlLabels {
    nodes: Vec<GqlLabel>,
}

#[derive(Deserialize)]
struct GqlLabel {
    name: String,
    color: String,
}

#[derive(Deserialize)]
struct GqlCommits {
    nodes: Vec<GqlCommitNode>,
}

#[derive(Deserialize)]
struct GqlCommitNode {
    commit: GqlCommit,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlCommit {
    status_check_rollup: Option<GqlRollup>,
}

#[derive(Deserialize)]
struct GqlRollup {
    state: String,
}

#[derive(Deserialize)]
struct ViewedFilesResponse {
    data: Option<ViewedFilesData>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

#[derive(Deserialize)]
struct ViewedFilesData {
    node: Option<ViewedFilesNode>,
}

#[derive(Deserialize)]
struct ViewedFilesNode {
    files: Option<ViewedFilesConn>,
}

#[derive(Deserialize)]
struct ViewedFilesConn {
    nodes: Vec<ViewedFileNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ViewedFileNode {
    path: String,
    viewer_viewed_state: String,
}

#[derive(Deserialize)]
struct MutationResponse {
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

#[derive(Deserialize)]
struct ReactionsResponse {
    data: Option<ReactionsData>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

#[derive(Deserialize)]
struct ReactionsData {
    repository: Option<ReactionsRepo>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReactionsRepo {
    pull_request: Option<ReactionsPr>,
}

#[derive(Deserialize)]
struct NodeList<T> {
    nodes: Vec<T>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReactionsPr {
    reviews: NodeList<ReactionNode>,
    comments: NodeList<ReactionNode>,
    review_threads: NodeList<ReviewThreadNode>,
}

#[derive(Deserialize)]
struct ReviewThreadNode {
    comments: NodeList<ReactionNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReactionNode {
    database_id: Option<u64>,
    id: String,
    #[serde(default)]
    reaction_groups: Vec<ReactionGroupNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReactionGroupNode {
    content: String,
    viewer_has_reacted: bool,
    reactors: ReactorsCount,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReactorsCount {
    total_count: u32,
}

impl ReactionNode {
    fn into_comment_reactions(self) -> Option<CommentReactions> {
        let database_id = self.database_id?;
        let reactions = self
            .reaction_groups
            .into_iter()
            .filter_map(|group| {
                Some(ReactionGroup {
                    content: ReactionContent::from_graphql(&group.content)?,
                    count: group.reactors.total_count,
                    viewer_reacted: group.viewer_has_reacted,
                })
            })
            .collect();
        Some(CommentReactions {
            database_id,
            node_id: self.id.into(),
            reactions,
        })
    }
}

fn map_check_rollup(state: &str) -> CheckRollup {
    match state {
        "SUCCESS" => CheckRollup::Success,
        "FAILURE" | "ERROR" => CheckRollup::Failure,
        _ => CheckRollup::Pending,
    }
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
        node_id: pr.id.into(),
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
        mergeable: match pr.mergeable.as_deref() {
            Some("MERGEABLE") => Some(true),
            Some("CONFLICTING") => Some(false),
            _ => None,
        },
        checks: pr
            .commits
            .and_then(|commits| commits.nodes.into_iter().next())
            .and_then(|node| node.commit.status_check_rollup)
            .map(|rollup| map_check_rollup(&rollup.state)),
        labels: pr
            .labels
            .map(|labels| {
                labels
                    .nodes
                    .into_iter()
                    .map(|label| PrLabel {
                        name: label.name.into(),
                        color: label.color.into(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        approvals: pr
            .latest_opinionated_reviews
            .map(|reviews| {
                reviews
                    .nodes
                    .iter()
                    .filter(|review| review.state == "APPROVED")
                    .count() as u32
            })
            .unwrap_or(0),
        required_approvals: pr
            .base_ref
            .and_then(|base_ref| base_ref.branch_protection_rule)
            .and_then(|rule| rule.required_approving_review_count),
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
                     id number title state createdAt updatedAt isDraft \
                     baseRefName headRefName baseRefOid headRefOid \
                     author {{ login }} reviewDecision \
                     mergeable \
                     labels(first: 10) {{ nodes {{ name color }} }} \
                     commits(last: 1) {{ nodes {{ commit {{ statusCheckRollup {{ state }} }} }} }} \
                     latestOpinionatedReviews(first: 50) {{ nodes {{ state }} }} \
                     baseRef {{ branchProtectionRule {{ requiredApprovingReviewCount }} }} \
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

    fn fetch_pull_request_body(
        &self,
        owner: &str,
        repo: &str,
        number: u32,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send>> {
        let url = format!("{GITHUB_API_URL}/repos/{owner}/{repo}/pulls/{number}");
        let http_client = self.http_client.clone();
        let token = self.token.clone();

        Box::pin(async move {
            let gh_pr: GhPullRequest = github_get(&http_client, &token, &url).await?;
            Ok(gh_pr.body.unwrap_or_default())
        })
    }

    fn fetch_viewed_files(
        &self,
        pr_node_id: &str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Vec<String>>> + Send>> {
        let query = "query($id: ID!) { \
               node(id: $id) { \
                 ... on PullRequest { files(first: 100) { nodes { path viewerViewedState } } } \
               } \
             }";
        let id = pr_node_id.to_string();
        let url = format!("{GITHUB_API_URL}/graphql");
        let http_client = self.http_client.clone();
        let token = self.token.clone();

        Box::pin(async move {
            let body = serde_json::json!({ "query": query, "variables": { "id": id } }).to_string();
            let response: ViewedFilesResponse =
                github_post(&http_client, &token, &url, body).await?;
            if !response.errors.is_empty() {
                let message = response
                    .errors
                    .into_iter()
                    .map(|error| error.message)
                    .collect::<Vec<_>>()
                    .join("; ");
                bail!("GitHub GraphQL error: {message}");
            }
            let viewed = response
                .data
                .and_then(|data| data.node)
                .and_then(|node| node.files)
                .map(|files| {
                    files
                        .nodes
                        .into_iter()
                        .filter(|file| file.viewer_viewed_state == "VIEWED")
                        .map(|file| file.path)
                        .collect()
                })
                .unwrap_or_default();
            Ok(viewed)
        })
    }

    fn mark_file_viewed(
        &self,
        pr_node_id: &str,
        path: &str,
        viewed: bool,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> {
        let mutation = if viewed {
            "markFileAsViewed"
        } else {
            "unmarkFileAsViewed"
        };
        let query = format!(
            "mutation($id: ID!, $path: String!) {{ \
               {mutation}(input: {{ pullRequestId: $id, path: $path }}) {{ clientMutationId }} \
             }}"
        );
        let id = pr_node_id.to_string();
        let path = path.to_string();
        let url = format!("{GITHUB_API_URL}/graphql");
        let http_client = self.http_client.clone();
        let token = self.token.clone();

        Box::pin(async move {
            let body = serde_json::json!({
                "query": query,
                "variables": { "id": id, "path": path },
            })
            .to_string();
            let response: MutationResponse = github_post(&http_client, &token, &url, body).await?;
            if !response.errors.is_empty() {
                let message = response
                    .errors
                    .into_iter()
                    .map(|error| error.message)
                    .collect::<Vec<_>>()
                    .join("; ");
                bail!("GitHub GraphQL error: {message}");
            }
            Ok(())
        })
    }

    fn fetch_comment_reactions(
        &self,
        owner: &str,
        repo: &str,
        number: u32,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Vec<CommentReactions>>> + Send>> {
        let groups = "reactionGroups { content viewerHasReacted reactors { totalCount } }";
        let query = format!(
            "query($owner: String!, $repo: String!, $number: Int!) {{ \
               repository(owner: $owner, name: $repo) {{ \
                 pullRequest(number: $number) {{ \
                   reviews(first: 100) {{ nodes {{ databaseId id {groups} }} }} \
                   comments(first: 100) {{ nodes {{ databaseId id {groups} }} }} \
                   reviewThreads(first: 100) {{ nodes {{ comments(first: 50) {{ nodes {{ databaseId id {groups} }} }} }} }} \
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
                "variables": { "owner": owner, "repo": repo, "number": number },
            })
            .to_string();
            let response: ReactionsResponse = github_post(&http_client, &token, &url, body).await?;
            if !response.errors.is_empty() {
                let message = response
                    .errors
                    .into_iter()
                    .map(|error| error.message)
                    .collect::<Vec<_>>()
                    .join("; ");
                bail!("GitHub GraphQL error: {message}");
            }
            let Some(pr) = response
                .data
                .and_then(|data| data.repository)
                .and_then(|repo| repo.pull_request)
            else {
                return Ok(Vec::new());
            };
            let reactions = pr
                .reviews
                .nodes
                .into_iter()
                .chain(pr.comments.nodes)
                .chain(
                    pr.review_threads
                        .nodes
                        .into_iter()
                        .flat_map(|thread| thread.comments.nodes),
                )
                .filter_map(ReactionNode::into_comment_reactions)
                .collect();
            Ok(reactions)
        })
    }

    fn set_reaction(
        &self,
        comment_node_id: &str,
        content: ReactionContent,
        add: bool,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> {
        let mutation = if add { "addReaction" } else { "removeReaction" };
        let query = format!(
            "mutation($id: ID!, $content: ReactionContent!) {{ \
               {mutation}(input: {{ subjectId: $id, content: $content }}) {{ clientMutationId }} \
             }}"
        );
        let id = comment_node_id.to_string();
        let content = content.graphql();
        let url = format!("{GITHUB_API_URL}/graphql");
        let http_client = self.http_client.clone();
        let token = self.token.clone();

        Box::pin(async move {
            let body = serde_json::json!({
                "query": query,
                "variables": { "id": id, "content": content },
            })
            .to_string();
            let response: MutationResponse = github_post(&http_client, &token, &url, body).await?;
            if !response.errors.is_empty() {
                let message = response
                    .errors
                    .into_iter()
                    .map(|error| error.message)
                    .collect::<Vec<_>>()
                    .join("; ");
                bail!("GitHub GraphQL error: {message}");
            }
            Ok(())
        })
    }

    fn fetch_pull_request_status(
        &self,
        owner: &str,
        repo: &str,
        number: u32,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<PullRequestStatus>> + Send>> {
        let query = "query($owner: String!, $repo: String!, $number: Int!) { \
               repository(owner: $owner, name: $repo) { \
                 pullRequest(number: $number) { \
                   mergeable \
                   labels(first: 20) { nodes { name color } } \
                   commits(last: 1) { nodes { commit { statusCheckRollup { state } } } } \
                 } \
               } \
             }";
        let owner = owner.to_string();
        let repo = repo.to_string();
        let url = format!("{GITHUB_API_URL}/graphql");
        let http_client = self.http_client.clone();
        let token = self.token.clone();

        Box::pin(async move {
            let body = serde_json::json!({
                "query": query,
                "variables": { "owner": owner, "repo": repo, "number": number },
            })
            .to_string();
            let response: PrStatusResponse = github_post(&http_client, &token, &url, body).await?;
            if !response.errors.is_empty() {
                let message = response
                    .errors
                    .into_iter()
                    .map(|error| error.message)
                    .collect::<Vec<_>>()
                    .join("; ");
                bail!("GitHub GraphQL error: {message}");
            }
            let Some(pr) = response
                .data
                .and_then(|data| data.repository)
                .and_then(|repository| repository.pull_request)
            else {
                return Ok(PullRequestStatus::default());
            };
            let mergeable = match pr.mergeable.as_deref() {
                Some("MERGEABLE") => Some(true),
                Some("CONFLICTING") => Some(false),
                _ => None,
            };
            let checks = pr
                .commits
                .and_then(|commits| commits.nodes.into_iter().next())
                .and_then(|node| node.commit.status_check_rollup)
                .map(|rollup| map_check_rollup(&rollup.state));
            let labels = pr
                .labels
                .map(|labels| {
                    labels
                        .nodes
                        .into_iter()
                        .map(|label| PrLabel {
                            name: label.name.into(),
                            color: label.color.into(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(PullRequestStatus {
                mergeable,
                checks,
                labels,
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
        // Conversation comments live on the issues API, distinct from inline
        // code comments on the pulls API.
        let issue_comments_url =
            format!("{GITHUB_API_URL}/repos/{owner}/{repo}/issues/{number}/comments?per_page=100");
        let http_client = self.http_client.clone();
        let token = self.token.clone();

        Box::pin(async move {
            // Fetch inline code comments
            let gh_comments: Vec<GhReviewComment> =
                github_get(&http_client, &token, &comments_url).await?;

            // Fetch top-level review submissions (approve, request changes, etc.)
            let gh_reviews: Vec<GhReview> = github_get(&http_client, &token, &reviews_url).await?;

            // Fetch the general PR conversation comments.
            let gh_issue_comments: Vec<GhIssueComment> =
                github_get(&http_client, &token, &issue_comments_url).await?;

            let mut comments: Vec<ReviewComment> =
                gh_comments.into_iter().map(map_review_comment).collect();

            // Conversation comments aren't anchored to a file/line, so they
            // surface in the general comments section.
            for issue_comment in gh_issue_comments {
                comments.push(ReviewComment {
                    id: issue_comment.id,
                    author: issue_comment.user.login.into(),
                    body: issue_comment.body.into(),
                    created_at: issue_comment.created_at.into(),
                    path: None,
                    line: None,
                    start_line: None,
                    reply_to: None,
                    diff_hunk: None,
                    node_id: SharedString::default(),
                    reactions: Vec::new(),
                });
            }

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
                            start_line: None,
                            reply_to: None,
                            diff_hunk: None,
                            node_id: SharedString::default(),
                            reactions: Vec::new(),
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
                start_line: None,
                reply_to: None,
                diff_hunk: None,
                node_id: SharedString::default(),
                reactions: Vec::new(),
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
