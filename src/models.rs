use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A comment from any GitHub source (issue comment or PR review comment),
/// normalised to the fields the PDF pipeline needs.
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct UnifiedComment {
    pub id: u64,
    pub timestamp: DateTime<Utc>,
    pub username: String,
    pub body: String,
    pub diff_hunk: Option<String>,
    pub url: String,
}

impl UnifiedComment {
    pub fn from_issue_comment(comment: IssueComment) -> Self {
        Self {
            id: comment.id,
            timestamp: comment.created_at,
            username: comment.user.login,
            body: comment.body.unwrap_or_default(),
            diff_hunk: None,
            url: comment.html_url,
        }
    }

    pub fn from_review_comment(comment: ReviewComment) -> Self {
        Self {
            id: comment.id,
            timestamp: comment.created_at,
            username: comment.user.map(|u| u.login).unwrap_or_default(),
            body: comment.body,
            diff_hunk: comment.diff_hunk,
            url: comment.html_url,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PRDiff {
    pub base_ref: String,
    pub head_ref: String,
    pub diff: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PRContext {
    pub head_branch: String,
    pub base_branch: String,
    pub diff: String,
    pub reviews: Vec<UnifiedComment>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct IssueContext {
    pub owner: String,
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub body: String,
    pub html_url: String,
    pub comments: Vec<UnifiedComment>,
    pub is_pr: bool,
    pub pr_context: Option<PRContext>,
    pub updated_at: String,
}

// ---------------------------------------------------------------------------
// GitHub REST API response types (only the fields we consume)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct Actor {
    pub login: String,
    #[serde(rename = "type", default)]
    pub kind: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    pub html_url: String,
    pub updated_at: DateTime<Utc>,
    /// Present (as an object with PR URLs) when the issue is a pull request.
    #[serde(default)]
    pub pull_request: Option<serde_json::Value>,
}

impl Issue {
    pub fn is_pr(&self) -> bool {
        self.pull_request.is_some()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct IssueComment {
    pub id: u64,
    pub created_at: DateTime<Utc>,
    pub user: Actor,
    pub body: Option<String>,
    pub html_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReviewComment {
    pub id: u64,
    pub created_at: DateTime<Utc>,
    pub user: Option<Actor>,
    pub body: String,
    pub diff_hunk: Option<String>,
    pub html_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GitRef {
    #[serde(rename = "ref")]
    pub ref_field: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub head: GitRef,
    pub base: GitRef,
}

// ---------------------------------------------------------------------------
// Webhook payload types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct Repository {
    pub name: String,
    pub owner: Actor,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Installation {
    pub id: u64,
}

/// The subset of every issue/PR webhook payload that gh2pdf needs.
/// `issue` is set for `issues` and `issue_comment` events; `pull_request`
/// for `pull_request*` events. Both carry the number we act on.
#[derive(Debug, Clone, Deserialize)]
pub struct WebhookPayload {
    pub action: Option<String>,
    pub issue: Option<NumberedTarget>,
    pub pull_request: Option<NumberedTarget>,
    pub repository: Option<Repository>,
    pub installation: Option<Installation>,
    pub sender: Option<Actor>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NumberedTarget {
    pub number: u64,
}

impl WebhookPayload {
    /// Returns the (owner, repo, issue-or-PR number, installation id) this
    /// event targets, or None when any part is missing.
    pub fn target(&self) -> Option<(String, String, u64, u64)> {
        let repo = self.repository.as_ref()?;
        let number = self
            .issue
            .as_ref()
            .or(self.pull_request.as_ref())
            .map(|t| t.number)?;
        let installation = self.installation.as_ref()?.id;
        Some((
            repo.owner.login.clone(),
            repo.name.clone(),
            number,
            installation,
        ))
    }
}
