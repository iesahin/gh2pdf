//! The end-to-end conversion pipeline: fetch an issue/PR, render it to PDF,
//! publish the PDF on the dedicated release, and link it from the
//! description. This is the single entry point both the webhook server and
//! the CLI use.

use crate::config::{PdfOptions, REPO_CONFIG_PATH};
use crate::description;
use crate::github::GitHubProvider;
use crate::models::{IssueContext, PRContext, PRDiff, UnifiedComment};
use crate::pdf;
use anyhow::{Context, Result};
use chrono::Utc;
use std::sync::Arc;

/// The result of a successful conversion.
#[derive(Debug, Clone)]
pub struct PublishedPdf {
    pub filename: String,
    pub url: String,
}

/// Converts issue/PR `number` of `owner/repo` to PDF, publishes it as an
/// asset of the release named by the options, and (unless disabled) links it
/// from the issue/PR description.
///
/// `options` are the server-wide defaults; a `.github/gh2pdf.toml` in the
/// target repository overrides them per repo.
pub async fn convert_and_publish(
    github: Arc<dyn GitHubProvider>,
    options: &PdfOptions,
    owner: &str,
    repo: &str,
    number: u64,
) -> Result<PublishedPdf> {
    let options = effective_options(github.clone(), options, owner, repo).await;
    let context = fetch_issue_context(github.clone(), &options, owner, repo, number).await?;

    log::info!(
        "Pipeline: Converting {}/{}#{} ({})",
        owner,
        repo,
        number,
        context.title
    );

    let pr_diff = context.pr_context.as_ref().and_then(|pr| {
        if options.include_diff {
            Some(PRDiff {
                base_ref: pr.base_branch.clone(),
                head_ref: pr.head_branch.clone(),
                diff: pr.diff.clone(),
            })
        } else {
            None
        }
    });

    let mut all_comments = context.comments.clone();
    if let Some(pr) = context.pr_context.as_ref() {
        all_comments.extend(pr.reviews.clone());
    }

    let md_content = pdf::assemble_markdown(
        Some(&context.body),
        all_comments,
        &options.omit_user,
        pr_diff,
        options.timezone_offset_hours,
    );

    let slug = pdf::slugify(&context.title);
    let output_name = format!("{}-{}-{}", context.repo, context.number, slug);
    let pdf_filename = format!("{}.pdf", output_name);

    let link_str = format!(
        "#link(\"{}\")[{}#{}]",
        context.html_url, context.repo, context.number
    );
    let author_field = author_field(&context, options.timezone_offset_hours);
    let preamble = pdf::build_preamble(&options, &context.title, &author_field, &link_str).await;

    // Each conversion gets its own scratch directory so concurrent runs
    // cannot clobber each other's intermediate files.
    let work_dir = tempfile_dir(&output_name)?;
    let token = github.token().await.ok();

    let pdf_path = pdf::compile_content_to_pdf(
        &md_content,
        &output_name,
        &preamble,
        "markdown",
        &work_dir,
        token.as_deref(),
    )
    .await
    .with_context(|| format!("compiling PDF for {}/{}#{}", owner, repo, number))?;

    let content = tokio::fs::read(&pdf_path)
        .await
        .with_context(|| format!("reading produced PDF {:?}", pdf_path))?;
    let _ = tokio::fs::remove_dir_all(&work_dir).await;

    // The stale prefix covers renamed titles: any previous PDF of this
    // issue/PR is removed before the new one is uploaded.
    let stale_prefix = format!("{}-{}-", context.repo, context.number);
    let url = github
        .publish_release_asset(
            owner,
            repo,
            &options.release_tag,
            &pdf_filename,
            &stale_prefix,
            content,
        )
        .await?;

    if options.link_description {
        let updated_at = Utc::now().format("%Y-%m-%d %H:%M UTC").to_string();
        let new_body = description::upsert_pdf_link(&context.body, &url, &updated_at);
        if new_body != context.body {
            github
                .update_issue_body(owner, repo, number, &new_body)
                .await
                .context("updating issue description with PDF link")?;
        }
    }

    log::info!("Pipeline: Published {} at {}", pdf_filename, url);
    Ok(PublishedPdf {
        filename: pdf_filename,
        url,
    })
}

/// Resolves the options in effect for a repository: server defaults patched
/// by the repo's `.github/gh2pdf.toml` when present. A broken or unreadable
/// repo config falls back to the defaults rather than blocking conversion.
pub async fn effective_options(
    github: Arc<dyn GitHubProvider>,
    defaults: &PdfOptions,
    owner: &str,
    repo: &str,
) -> PdfOptions {
    match github.get_repo_file(owner, repo, REPO_CONFIG_PATH).await {
        Ok(Some(content)) => match toml::from_str(&content) {
            Ok(patch) => defaults.with_patch(patch),
            Err(e) => {
                log::warn!(
                    "Pipeline: Invalid {} in {}/{}: {}; using defaults",
                    REPO_CONFIG_PATH,
                    owner,
                    repo,
                    e
                );
                defaults.clone()
            }
        },
        Ok(None) => defaults.clone(),
        Err(e) => {
            log::warn!(
                "Pipeline: Could not fetch {} from {}/{}: {}; using defaults",
                REPO_CONFIG_PATH,
                owner,
                repo,
                e
            );
            defaults.clone()
        }
    }
}

/// Fetches everything the PDF needs about an issue or PR: body, comments,
/// and — for PRs — review comments, branches, and the diff.
pub async fn fetch_issue_context(
    github: Arc<dyn GitHubProvider>,
    options: &PdfOptions,
    owner: &str,
    repo: &str,
    number: u64,
) -> Result<IssueContext> {
    let issue = github.get_issue(owner, repo, number).await?;

    let mut unified_comments = Vec::new();
    for c in github.get_issue_comments(owner, repo, number).await? {
        unified_comments.push(UnifiedComment::from_issue_comment(c));
    }

    let mut pr_context = None;
    if issue.is_pr() {
        let mut review_comments = Vec::new();
        for r in github.get_review_comments(owner, repo, number).await? {
            review_comments.push(UnifiedComment::from_review_comment(r));
        }

        if let Some(pr) = github.get_pull_request(owner, repo, number).await? {
            let diff = if options.include_diff {
                github
                    .get_pull_request_diff(owner, repo, number)
                    .await
                    .unwrap_or_default()
            } else {
                String::new()
            };

            pr_context = Some(PRContext {
                head_branch: pr.head.ref_field,
                base_branch: pr.base.ref_field,
                diff,
                reviews: review_comments,
            });
        }
    }

    Ok(IssueContext {
        owner: owner.to_string(),
        repo: repo.to_string(),
        number: issue.number,
        title: issue.title,
        body: issue.body.unwrap_or_default(),
        html_url: issue.html_url,
        comments: unified_comments,
        is_pr: issue.pull_request.is_some(),
        pr_context,
        updated_at: issue.updated_at.to_rfc3339(),
    })
}

/// PRs show their head branch as the author field; issues show their last
/// update time (matches inboxbot's PDF header format).
fn author_field(context: &IssueContext, tz_offset_hours: i32) -> String {
    if context.is_pr {
        context
            .pr_context
            .as_ref()
            .map(|pr| pr.head_branch.clone())
            .unwrap_or_else(|| format!("PR #{}", context.number))
    } else if let Ok(updated_dt) = chrono::DateTime::parse_from_rfc3339(&context.updated_at) {
        let offset = chrono::FixedOffset::east_opt(tz_offset_hours * 3600)
            .unwrap_or_else(|| chrono::FixedOffset::east_opt(0).unwrap());
        updated_dt
            .with_timezone(&offset)
            .format("%Y-%m-%d %H:%M")
            .to_string()
    } else {
        context.updated_at.clone()
    }
}

fn tempfile_dir(output_name: &str) -> Result<std::path::PathBuf> {
    let dir = std::env::temp_dir().join(format!("gh2pdf-{}-{}", output_name, std::process::id()));
    std::fs::create_dir_all(&dir).with_context(|| format!("creating work dir {:?}", dir))?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::mock::MockGitHubClient;
    use crate::models::{Actor, GitRef, Issue, IssueComment, PullRequest, ReviewComment};
    use chrono::{DateTime, Utc};

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn sample_issue(is_pr: bool) -> Issue {
        Issue {
            number: 7,
            title: "Add Feature X".to_string(),
            body: Some("Feature description".to_string()),
            html_url: "https://github.com/o/r/issues/7".to_string(),
            updated_at: ts("2026-07-19T09:00:00Z"),
            pull_request: is_pr.then(|| serde_json::json!({"url": "x"})),
        }
    }

    #[tokio::test]
    async fn test_fetch_issue_context_for_issue() {
        let mock = Arc::new(MockGitHubClient::default());
        *mock.issue.lock().unwrap() = Some(sample_issue(false));
        mock.issue_comments.lock().unwrap().push(IssueComment {
            id: 1,
            created_at: ts("2026-07-19T08:00:00Z"),
            user: Actor {
                login: "alice".into(),
                kind: "User".into(),
            },
            body: Some("A comment".into()),
            html_url: "https://github.com/o/r/issues/7#c1".into(),
        });

        let ctx = fetch_issue_context(mock, &PdfOptions::default(), "o", "r", 7)
            .await
            .unwrap();
        assert_eq!(ctx.number, 7);
        assert!(!ctx.is_pr);
        assert!(ctx.pr_context.is_none());
        assert_eq!(ctx.comments.len(), 1);
        assert_eq!(ctx.comments[0].username, "alice");
    }

    #[tokio::test]
    async fn test_fetch_issue_context_for_pr_includes_diff_and_reviews() {
        let mock = Arc::new(MockGitHubClient::default());
        *mock.issue.lock().unwrap() = Some(sample_issue(true));
        *mock.pull_request.lock().unwrap() = Some(PullRequest {
            number: 7,
            head: GitRef {
                ref_field: "feature".into(),
            },
            base: GitRef {
                ref_field: "main".into(),
            },
        });
        *mock.diff.lock().unwrap() = "@@ diff @@".to_string();
        mock.review_comments.lock().unwrap().push(ReviewComment {
            id: 2,
            created_at: ts("2026-07-19T08:30:00Z"),
            user: Some(Actor {
                login: "bob".into(),
                kind: "User".into(),
            }),
            body: "Review note".into(),
            diff_hunk: Some("@@ hunk @@".into()),
            html_url: "https://github.com/o/r/pull/7#rc2".into(),
        });

        let ctx = fetch_issue_context(mock, &PdfOptions::default(), "o", "r", 7)
            .await
            .unwrap();
        assert!(ctx.is_pr);
        let pr = ctx.pr_context.unwrap();
        assert_eq!(pr.head_branch, "feature");
        assert_eq!(pr.base_branch, "main");
        assert_eq!(pr.diff, "@@ diff @@");
        assert_eq!(pr.reviews.len(), 1);
    }

    #[tokio::test]
    async fn test_fetch_issue_context_skips_diff_when_disabled() {
        let mock = Arc::new(MockGitHubClient::default());
        *mock.issue.lock().unwrap() = Some(sample_issue(true));
        *mock.pull_request.lock().unwrap() = Some(PullRequest {
            number: 7,
            head: GitRef {
                ref_field: "feature".into(),
            },
            base: GitRef {
                ref_field: "main".into(),
            },
        });
        *mock.diff.lock().unwrap() = "@@ diff @@".to_string();

        let options = PdfOptions {
            include_diff: false,
            ..Default::default()
        };
        let ctx = fetch_issue_context(mock, &options, "o", "r", 7)
            .await
            .unwrap();
        assert_eq!(ctx.pr_context.unwrap().diff, "");
    }

    #[tokio::test]
    async fn test_effective_options_applies_repo_config() {
        let mock = Arc::new(MockGitHubClient::default());
        mock.repo_files.lock().unwrap().insert(
            REPO_CONFIG_PATH.to_string(),
            "release_tag = \"custom-pdfs\"\nomit_user = \"iesahin\"".to_string(),
        );

        let opts = effective_options(mock, &PdfOptions::default(), "o", "r").await;
        assert_eq!(opts.release_tag, "custom-pdfs");
        assert_eq!(opts.omit_user, "iesahin");
        // Untouched options keep the server defaults.
        assert!(opts.include_diff);
    }

    #[tokio::test]
    async fn test_effective_options_ignores_broken_repo_config() {
        let mock = Arc::new(MockGitHubClient::default());
        mock.repo_files.lock().unwrap().insert(
            REPO_CONFIG_PATH.to_string(),
            "this is not [valid toml".to_string(),
        );

        let opts = effective_options(mock, &PdfOptions::default(), "o", "r").await;
        assert_eq!(opts.release_tag, "gh2pdf");
    }

    #[test]
    fn test_author_field_for_issue_uses_updated_time() {
        let ctx = IssueContext {
            owner: "o".into(),
            repo: "r".into(),
            number: 7,
            title: "T".into(),
            body: String::new(),
            html_url: String::new(),
            comments: vec![],
            is_pr: false,
            pr_context: None,
            updated_at: "2026-07-19T09:00:00+00:00".into(),
        };
        assert_eq!(author_field(&ctx, 3), "2026-07-19 12:00");
    }

    #[test]
    fn test_author_field_for_pr_uses_head_branch() {
        let ctx = IssueContext {
            owner: "o".into(),
            repo: "r".into(),
            number: 7,
            title: "T".into(),
            body: String::new(),
            html_url: String::new(),
            comments: vec![],
            is_pr: true,
            pr_context: Some(PRContext {
                head_branch: "feature".into(),
                base_branch: "main".into(),
                diff: String::new(),
                reviews: vec![],
            }),
            updated_at: "2026-07-19T09:00:00+00:00".into(),
        };
        assert_eq!(author_field(&ctx, 3), "feature");
    }
}
