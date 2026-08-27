//! Built-in GitHub jobs.
//!
//! [`Activity`] snapshots your GitHub activity into a typed JSON file: issues
//! and pull requests you opened, pull requests waiting on your review, repos
//! you starred, and the stars your own repos received.
//!
//! It all comes from one GraphQL request. The REST equivalent would need five
//! round trips and would still not give the "review requested" list without a
//! search call.
//!
//! Only public data is exported. That is enforced here rather than left to the
//! token's scopes, so granting the token `repo` access later for some other
//! reason cannot silently start leaking private repository names into a file
//! you may well be publishing.

use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, USER_AGENT};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::builtins::write_json;
use crate::config::GitHubSettings;
use crate::job::{Job, JobContext, JobReport, JobResult};

/// GitHub's GraphQL endpoint.
const GRAPHQL_URL: &str = "https://api.github.com/graphql";

/// One request covering everything the snapshot needs.
///
/// `repositories` is ordered by stars so that, if the account has more repos
/// than one page holds, the ones that dominate the star total are the ones
/// that make it in.
const QUERY: &str = r#"
query($issues: Int!, $prs: Int!, $starred: Int!, $repos: Int!, $reviews: Int!) {
  rateLimit { remaining resetAt }
  viewer {
    login
    issues(first: $issues, orderBy: {field: CREATED_AT, direction: DESC}) {
      totalCount
      nodes {
        number title url state createdAt updatedAt
        comments { totalCount }
        repository { nameWithOwner isPrivate }
      }
    }
    pullRequests(first: $prs, orderBy: {field: CREATED_AT, direction: DESC}) {
      totalCount
      nodes {
        number title url state createdAt updatedAt mergedAt additions deletions
        repository { nameWithOwner isPrivate }
      }
    }
    starredRepositories(first: $starred, orderBy: {field: STARRED_AT, direction: DESC}) {
      totalCount
      edges {
        starredAt
        node {
          nameWithOwner url description stargazerCount
          primaryLanguage { name }
        }
      }
    }
    repositories(
      first: $repos
      privacy: PUBLIC
      ownerAffiliations: OWNER
      isFork: false
      orderBy: {field: STARGAZERS, direction: DESC}
    ) {
      totalCount
      nodes {
        nameWithOwner url description stargazerCount forkCount
        primaryLanguage { name }
      }
    }
  }
  search(query: "is:pr is:open is:public review-requested:@me archived:false", type: ISSUE, first: $reviews) {
    issueCount
    nodes {
      ... on PullRequest {
        number title url createdAt
        author { login }
        repository { nameWithOwner }
      }
    }
  }
}
"#;

/// Snapshots GitHub activity to a JSON file.
///
/// Arguments: `filename` (default `github_activity.json`), `issues_limit`
/// (20), `prs_limit` (20), `starred_limit` (30), `repos_limit` (100),
/// `review_requests_limit` (20).
pub struct Activity {
    settings: Arc<GitHubSettings>,
    /// Reused across runs so connections are pooled.
    http: reqwest::Client,
}

impl Activity {
    /// Builds the job from the GitHub settings.
    pub fn new(settings: Arc<GitHubSettings>) -> anyhow::Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static("vps-cron"));
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", settings.token))
                .context("GITHUB_TOKEN contains characters that cannot go in a header")?,
        );

        let http = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .context("Failed to build the GitHub HTTP client")?;

        Ok(Self { settings, http })
    }
}

#[async_trait]
impl Job for Activity {
    async fn run(&self, ctx: &JobContext<'_>) -> JobResult {
        let filename = ctx.arg_str("filename").unwrap_or("github_activity.json");

        let variables = serde_json::json!({
            "issues": ctx.arg_u32("issues_limit").unwrap_or(20),
            "prs": ctx.arg_u32("prs_limit").unwrap_or(20),
            "starred": ctx.arg_u32("starred_limit").unwrap_or(30),
            "repos": ctx.arg_u32("repos_limit").unwrap_or(100),
            "reviews": ctx.arg_u32("review_requests_limit").unwrap_or(20),
        });

        let response = self.fetch(&variables).await?;
        let snapshot = Snapshot::from_response(response);

        let path = write_json(&self.settings.destination_folder, filename, &snapshot)?;

        Ok(JobReport::summary(format!(
            "{} issues, {} PRs, {} review requests, {} starred, {} stars received -> {path}",
            snapshot.issues.len(),
            snapshot.pull_requests.len(),
            snapshot.review_requests.len(),
            snapshot.starred.len(),
            snapshot.repositories.stars_received,
        )))
    }
}

impl Activity {
    /// Sends the GraphQL query and unwraps the payload.
    async fn fetch(&self, variables: &serde_json::Value) -> anyhow::Result<Data> {
        let response = self
            .http
            .post(GRAPHQL_URL)
            .json(&serde_json::json!({ "query": QUERY, "variables": variables }))
            .send()
            .await
            .context("Failed to reach the GitHub GraphQL API")?;

        let status = response.status();
        let body = response
            .text()
            .await
            .context("Failed to read the GitHub response")?;

        if !status.is_success() {
            bail!("GitHub returned {status}: {}", body.trim());
        }

        let envelope: Envelope =
            serde_json::from_str(&body).context("Failed to parse the GitHub response")?;

        // GraphQL reports query errors inside a 200 response, so the status
        // code alone is not enough to know the request worked.
        if let Some(errors) = envelope.errors.filter(|e| !e.is_empty()) {
            let joined = errors
                .iter()
                .map(|e| e.message.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            bail!("GitHub GraphQL error: {joined}");
        }

        envelope
            .data
            .ok_or_else(|| anyhow!("GitHub returned no data and no errors"))
    }
}

// ---------------------------------------------------------------------------
// Exported shape
// ---------------------------------------------------------------------------

/// The JSON document written to disk.
#[derive(Debug, Serialize)]
pub struct Snapshot {
    /// When this snapshot was taken.
    pub fetched_at: DateTime<Utc>,
    /// The authenticated account.
    pub login: String,
    /// Issues you opened, newest first.
    pub issues: Vec<Issue>,
    /// Pull requests you opened, newest first.
    pub pull_requests: Vec<PullRequest>,
    /// Open pull requests waiting on your review.
    pub review_requests: Vec<ReviewRequest>,
    /// Repositories you starred, most recently starred first.
    pub starred: Vec<Starred>,
    /// Your own repositories and the stars they received.
    pub repositories: Repositories,
    /// What is left of the GraphQL rate limit.
    pub rate_limit: RateLimit,
}

/// An issue you opened.
#[derive(Debug, Serialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub url: String,
    /// `OPEN` or `CLOSED`.
    pub state: String,
    pub repository: String,
    pub comments: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A pull request you opened.
#[derive(Debug, Serialize)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub url: String,
    /// `OPEN`, `CLOSED` or `MERGED`.
    pub state: String,
    pub repository: String,
    pub additions: u64,
    pub deletions: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// When it was merged, for merged pull requests.
    pub merged_at: Option<DateTime<Utc>>,
}

/// An open pull request awaiting your review.
#[derive(Debug, Serialize)]
pub struct ReviewRequest {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub repository: String,
    /// `None` when the author's account is gone.
    pub author: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// A repository you starred.
#[derive(Debug, Serialize)]
pub struct Starred {
    pub repository: String,
    pub url: String,
    pub description: Option<String>,
    pub stars: u64,
    pub language: Option<String>,
    pub starred_at: DateTime<Utc>,
}

/// Your own repositories, with the stars they received.
#[derive(Debug, Serialize)]
pub struct Repositories {
    /// How many public non-fork repositories you own.
    pub total: u64,
    /// How many of them this snapshot covers.
    ///
    /// Below `total` when you own more repositories than `repos_limit`, which
    /// also makes `stars_received` a lower bound.
    pub counted: u64,
    /// Stars across the counted repositories.
    pub stars_received: u64,
    /// The repositories themselves, most starred first.
    pub items: Vec<Repository>,
}

/// One of your repositories.
#[derive(Debug, Serialize)]
pub struct Repository {
    pub repository: String,
    pub url: String,
    pub description: Option<String>,
    pub stars: u64,
    pub forks: u64,
    pub language: Option<String>,
}

/// What is left of the GraphQL rate limit.
#[derive(Debug, Serialize)]
pub struct RateLimit {
    pub remaining: u64,
    pub resets_at: DateTime<Utc>,
}

impl Snapshot {
    /// Converts the GraphQL payload into the exported shape.
    ///
    /// Private issues and pull requests are dropped here even though the query
    /// is expected to return none, so the export stays public whatever the
    /// token can see.
    fn from_response(data: Data) -> Self {
        let viewer = data.viewer;

        let items: Vec<Repository> = viewer
            .repositories
            .nodes
            .into_iter()
            .map(|node| Repository {
                repository: node.name_with_owner,
                url: node.url,
                description: node.description,
                stars: node.stargazer_count,
                forks: node.fork_count,
                language: node.primary_language.map(|l| l.name),
            })
            .collect();

        Self {
            fetched_at: Utc::now(),
            login: viewer.login,
            issues: viewer
                .issues
                .nodes
                .into_iter()
                .filter(|node| !node.repository.is_private)
                .map(|node| Issue {
                    number: node.number,
                    title: node.title,
                    url: node.url,
                    state: node.state,
                    repository: node.repository.name_with_owner,
                    comments: node.comments.total_count,
                    created_at: node.created_at,
                    updated_at: node.updated_at,
                })
                .collect(),
            pull_requests: viewer
                .pull_requests
                .nodes
                .into_iter()
                .filter(|node| !node.repository.is_private)
                .map(|node| PullRequest {
                    number: node.number,
                    title: node.title,
                    url: node.url,
                    state: node.state,
                    repository: node.repository.name_with_owner,
                    additions: node.additions,
                    deletions: node.deletions,
                    created_at: node.created_at,
                    updated_at: node.updated_at,
                    merged_at: node.merged_at,
                })
                .collect(),
            review_requests: data
                .search
                .nodes
                .into_iter()
                .filter_map(|node| {
                    Some(ReviewRequest {
                        number: node.number?,
                        title: node.title?,
                        url: node.url?,
                        repository: node.repository?.name_with_owner,
                        author: node.author.and_then(|a| a.login),
                        created_at: node.created_at?,
                    })
                })
                .collect(),
            starred: viewer
                .starred_repositories
                .edges
                .into_iter()
                .map(|edge| Starred {
                    repository: edge.node.name_with_owner,
                    url: edge.node.url,
                    description: edge.node.description,
                    stars: edge.node.stargazer_count,
                    language: edge.node.primary_language.map(|l| l.name),
                    starred_at: edge.starred_at,
                })
                .collect(),
            repositories: Repositories {
                total: viewer.repositories.total_count,
                counted: items.len() as u64,
                stars_received: items.iter().map(|r| r.stars).sum(),
                items,
            },
            rate_limit: RateLimit {
                remaining: data.rate_limit.remaining,
                resets_at: data.rate_limit.reset_at,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// GraphQL response
// ---------------------------------------------------------------------------

/// The GraphQL envelope, which carries data, errors, or both.
#[derive(Debug, Deserialize)]
struct Envelope {
    data: Option<Data>,
    errors: Option<Vec<GraphQlError>>,
}

/// One entry from the GraphQL `errors` array.
#[derive(Debug, Deserialize)]
struct GraphQlError {
    message: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Data {
    rate_limit: RawRateLimit,
    viewer: Viewer,
    search: Search,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRateLimit {
    remaining: u64,
    reset_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Viewer {
    login: String,
    issues: Nodes<IssueNode>,
    pull_requests: Nodes<PullRequestNode>,
    starred_repositories: Edges<StarredEdge>,
    repositories: Nodes<RepositoryNode>,
}

/// A GraphQL connection exposed through `nodes`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Nodes<T> {
    total_count: u64,
    nodes: Vec<T>,
}

/// A GraphQL connection exposed through `edges`, needed for `starredAt`.
#[derive(Debug, Deserialize)]
struct Edges<T> {
    edges: Vec<T>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IssueNode {
    number: u64,
    title: String,
    url: String,
    state: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    comments: TotalCount,
    repository: RepositoryRef,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequestNode {
    number: u64,
    title: String,
    url: String,
    state: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    merged_at: Option<DateTime<Utc>>,
    additions: u64,
    deletions: u64,
    repository: RepositoryRef,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepositoryNode {
    name_with_owner: String,
    url: String,
    description: Option<String>,
    stargazer_count: u64,
    fork_count: u64,
    primary_language: Option<Language>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StarredEdge {
    starred_at: DateTime<Utc>,
    node: StarredNode,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StarredNode {
    name_with_owner: String,
    url: String,
    description: Option<String>,
    stargazer_count: u64,
    primary_language: Option<Language>,
}

#[derive(Debug, Deserialize)]
struct Search {
    nodes: Vec<SearchNode>,
}

/// A search hit.
///
/// Every field is optional because `search` returns a union: a node that is
/// not a pull request comes back as an empty object rather than an error.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchNode {
    number: Option<u64>,
    title: Option<String>,
    url: Option<String>,
    created_at: Option<DateTime<Utc>>,
    author: Option<Author>,
    repository: Option<RepositoryRef>,
}

#[derive(Debug, Deserialize)]
struct Author {
    /// Absent when the account has been deleted.
    login: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepositoryRef {
    name_with_owner: String,
    #[serde(default)]
    is_private: bool,
}

#[derive(Debug, Deserialize)]
struct TotalCount {
    #[serde(rename = "totalCount")]
    total_count: u64,
}

#[derive(Debug, Deserialize)]
struct Language {
    name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A payload shaped like a real GitHub response, including the awkward
    /// parts: a private repository, a deleted author and a null language.
    const SAMPLE: &str = r#"{
      "data": {
        "rateLimit": { "remaining": 4987, "resetAt": "2026-08-27T16:00:00Z" },
        "viewer": {
          "login": "octocat",
          "issues": {
            "totalCount": 2,
            "nodes": [
              { "number": 7, "title": "Public issue", "url": "https://x/7", "state": "OPEN",
                "createdAt": "2026-08-01T10:00:00Z", "updatedAt": "2026-08-02T10:00:00Z",
                "comments": { "totalCount": 3 },
                "repository": { "nameWithOwner": "octocat/pub", "isPrivate": false } },
              { "number": 9, "title": "Secret issue", "url": "https://x/9", "state": "OPEN",
                "createdAt": "2026-08-01T10:00:00Z", "updatedAt": "2026-08-02T10:00:00Z",
                "comments": { "totalCount": 0 },
                "repository": { "nameWithOwner": "octocat/priv", "isPrivate": true } }
            ]
          },
          "pullRequests": {
            "totalCount": 1,
            "nodes": [
              { "number": 12, "title": "Add thing", "url": "https://x/12", "state": "MERGED",
                "createdAt": "2026-08-03T10:00:00Z", "updatedAt": "2026-08-04T10:00:00Z",
                "mergedAt": "2026-08-04T10:00:00Z", "additions": 40, "deletions": 5,
                "repository": { "nameWithOwner": "octocat/pub", "isPrivate": false } }
            ]
          },
          "starredRepositories": {
            "totalCount": 1,
            "edges": [
              { "starredAt": "2026-08-20T10:00:00Z",
                "node": { "nameWithOwner": "rust-lang/rust", "url": "https://x/rust",
                          "description": null, "stargazerCount": 99000,
                          "primaryLanguage": { "name": "Rust" } } }
            ]
          },
          "repositories": {
            "totalCount": 12,
            "nodes": [
              { "nameWithOwner": "octocat/pub", "url": "https://x/pub", "description": "d",
                "stargazerCount": 100, "forkCount": 4, "primaryLanguage": { "name": "Rust" } },
              { "nameWithOwner": "octocat/other", "url": "https://x/other", "description": null,
                "stargazerCount": 37, "forkCount": 0, "primaryLanguage": null }
            ]
          }
        },
        "search": {
          "issueCount": 1,
          "nodes": [
            { "number": 3, "title": "Review me", "url": "https://x/3",
              "createdAt": "2026-08-25T10:00:00Z", "author": { "login": null },
              "repository": { "nameWithOwner": "someone/repo" } }
          ]
        }
      }
    }"#;

    fn snapshot() -> Snapshot {
        let envelope: Envelope = serde_json::from_str(SAMPLE).expect("sample should parse");
        Snapshot::from_response(envelope.data.expect("sample has data"))
    }

    #[test]
    fn private_issues_are_dropped() {
        let snapshot = snapshot();
        assert_eq!(snapshot.issues.len(), 1, "the private issue should be gone");
        assert_eq!(snapshot.issues[0].repository, "octocat/pub");
        assert_eq!(snapshot.issues[0].comments, 3);
    }

    #[test]
    fn merged_pull_requests_keep_their_merge_date() {
        let pr = &snapshot().pull_requests[0];
        assert_eq!(pr.state, "MERGED");
        assert!(pr.merged_at.is_some());
        assert_eq!((pr.additions, pr.deletions), (40, 5));
    }

    #[test]
    fn stars_received_sums_the_counted_repositories() {
        let repos = snapshot().repositories;
        assert_eq!(repos.stars_received, 137);
        assert_eq!(repos.counted, 2);
        assert_eq!(repos.total, 12, "total comes from GitHub, not from the page");
        assert_eq!(repos.items[1].language, None, "a null language is allowed");
    }

    #[test]
    fn a_deleted_author_does_not_drop_the_review_request() {
        let reviews = snapshot().review_requests;
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].author, None);
        assert_eq!(reviews[0].repository, "someone/repo");
    }

    #[test]
    fn starred_repositories_keep_the_star_date() {
        let starred = snapshot().starred;
        assert_eq!(starred[0].repository, "rust-lang/rust");
        assert_eq!(starred[0].language.as_deref(), Some("Rust"));
        assert_eq!(starred[0].starred_at.to_rfc3339(), "2026-08-20T10:00:00+00:00");
    }

    #[test]
    fn graphql_errors_are_surfaced_even_with_a_200() {
        let body = r#"{"data": null, "errors": [{"message": "Bad credentials"}]}"#;
        let envelope: Envelope = serde_json::from_str(body).expect("should parse");
        assert!(envelope.data.is_none());
        assert_eq!(envelope.errors.unwrap()[0].message, "Bad credentials");
    }

    #[test]
    fn the_exported_json_uses_snake_case_keys() {
        let json = serde_json::to_value(snapshot()).expect("should serialise");
        for key in ["fetched_at", "pull_requests", "review_requests", "repositories", "rate_limit"] {
            assert!(json.get(key).is_some(), "missing key '{key}'");
        }
        assert!(json["repositories"].get("stars_received").is_some());
    }
}
