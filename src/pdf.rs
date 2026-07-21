//! The PDF pipeline: assemble Markdown from an issue/PR, convert it to Typst
//! with Pandoc, post-process the Typst source (page breaks, mermaid, remote
//! images), and compile it to PDF with Typst. Ported from inboxbot's
//! `pdf.rs` so the output format is identical.

use crate::config::PdfOptions;
use crate::models::{PRDiff, UnifiedComment};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, FixedOffset, Utc};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::process::Command;

/// Derives a deterministic, filesystem-safe image filename from a remote URL.
///
/// The name is `<8-hex-hash>_<original-basename>`, where the hash covers the
/// full URL.  This guarantees:
/// - Same URL  → same filename on every run (idempotent, cache-friendly).
/// - Different URLs that happen to share a basename → different filenames
///   (no collision counter needed, no in-memory state required).
fn url_to_image_filename(url: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut hasher);
    let hash = hasher.finish();

    let basename = url
        .split('?')
        .next()
        .unwrap_or(url)
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty() && s.len() < 200)
        .unwrap_or("image.png");

    format!("{:08x}_{}", hash & 0xFFFF_FFFF, basename)
}

/// Generates a slug from a string (lowercase, replace non-alphanumeric with hyphens).
pub fn slugify(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// Groups comments into buckets based on a 2-minute temporal gap.
pub fn group_comments(comments: Vec<UnifiedComment>) -> Vec<Vec<UnifiedComment>> {
    let mut sorted_comments = comments;
    sorted_comments.sort_by_key(|c| c.timestamp);

    let mut groups: Vec<Vec<UnifiedComment>> = Vec::new();
    for comment in sorted_comments {
        if let Some(last_group) = groups.last_mut() {
            if let Some(last_comment) = last_group.last() {
                let gap = comment.timestamp - last_comment.timestamp;
                if gap <= chrono::Duration::minutes(2) {
                    last_group.push(comment);
                    continue;
                }
            }
        }
        groups.push(vec![comment]);
    }
    groups
}

fn local_offset(tz_offset_hours: i32) -> FixedOffset {
    FixedOffset::east_opt(tz_offset_hours * 3600)
        .unwrap_or_else(|| FixedOffset::east_opt(0).unwrap())
}

/// Formats a UTC timestamp as a local time string.
fn format_local_time(timestamp: &DateTime<Utc>, tz_offset_hours: i32) -> String {
    timestamp
        .with_timezone(&local_offset(tz_offset_hours))
        .format("%H:%M")
        .to_string()
}

/// Formats a UTC timestamp as a local date + time string.
fn format_local_datetime(timestamp: &DateTime<Utc>, tz_offset_hours: i32) -> String {
    timestamp
        .with_timezone(&local_offset(tz_offset_hours))
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

/// Assembles the Markdown content for an issue or PR.
///
/// Comment groups are always ordered from most recent to oldest; the first
/// comment of each group is timestamped with its full date so the reader
/// can tell groups on different days apart.
pub fn assemble_markdown(
    body: Option<&str>,
    comments: Vec<UnifiedComment>,
    user_to_omit: &str,
    pr_diff: Option<PRDiff>,
    tz_offset_hours: i32,
) -> String {
    let mut md = String::new();
    if let Some(body_text) = body {
        md.push_str(body_text);
        md.push_str("\n\nPAGEBREAKPLACEHOLDER\n\n");
    }

    let mut groups = group_comments(comments);
    groups.reverse();

    for (i, group) in groups.into_iter().enumerate() {
        if i > 0 {
            md.push_str("PAGEBREAKPLACEHOLDER\n\n");
        }

        for (j, comment) in group.into_iter().enumerate() {
            let username_part = if comment.username == user_to_omit {
                String::new()
            } else {
                format!(" - {}", comment.username)
            };

            let time_part = if j == 0 {
                format_local_datetime(&comment.timestamp, tz_offset_hours)
            } else {
                format_local_time(&comment.timestamp, tz_offset_hours)
            };

            md.push_str(&format!(
                "**[{}{}]({})**\n\n",
                time_part, username_part, comment.url
            ));

            if let Some(diff) = comment.diff_hunk {
                md.push_str("```diff\n");
                md.push_str(&diff);
                md.push_str("\n```\n\n");
            }

            md.push_str(&comment.body);
            md.push_str("\n\n");
        }
    }

    if let Some(diff_info) = pr_diff {
        if !md.is_empty() && !md.ends_with("PAGEBREAKPLACEHOLDER\n\n") {
            md.push_str("PAGEBREAKPLACEHOLDER\n\n");
        }
        md.push_str(&format!(
            "## PR Diff: {} <- {}\n\n",
            diff_info.base_ref, diff_info.head_ref
        ));
        md.push_str("```diff\n");
        md.push_str(&diff_info.diff);
        md.push_str("\n```\n\n");
    }

    md
}

/// Returns the Typst preamble for a document, with title, author and date
/// (link) fields filled in.
///
/// When `options.template_path` points to a readable file it is used as the
/// template; otherwise a built-in template (using the same `toffee-tufte`
/// layout as inboxbot) is parameterised with the paper/font options.
pub async fn build_preamble(
    options: &PdfOptions,
    title_str: &str,
    author_str: &str,
    date_str: &str,
) -> String {
    let template = match &options.template_path {
        Some(path) => match tokio::fs::read_to_string(path).await {
            Ok(content) => content,
            Err(e) => {
                log::warn!(
                    "PDF: Could not read template {}: {}; using built-in template",
                    path,
                    e
                );
                builtin_template(options)
            }
        },
        None => builtin_template(options),
    };
    template
        .replace("TITLE_PLACEHOLDER", title_str)
        .replace("AUTHOR_PLACEHOLDER", author_str)
        .replace("DATE_PLACEHOLDER", date_str)
}

fn builtin_template(options: &PdfOptions) -> String {
    format!(
        r#"
#import "@preview/toffee-tufte:0.1.1": *
#import "@preview/codedis:0.3.0": *

#set page(paper: "{}", margin: 1cm)
#set text(font: "{}", size: {}pt)

#let blockquote(content) = quote(block: true, content)
#let horizontalrule = align(center)[#v(0.5em)#text(size: 18pt)[⁂]#v(0.5em)]

#show: template.with(
  title: [TITLE_PLACEHOLDER],
  authors: "AUTHOR_PLACEHOLDER",
  date: [DATE_PLACEHOLDER],
)
"#,
        options.paper, options.font, options.font_size_pt
    )
}

/// Pre-processes Markdown content before passing it to Pandoc.
/// Specifically, it converts HTML <img> tags to standard Markdown image syntax,
/// as Pandoc's Typst writer tends to ignore raw HTML tags.
pub fn preprocess_markdown(content: &str) -> String {
    let img_tag_re =
        Regex::new(r#"(?i)<img\s+[^>]*src=["']([^"']+)["'][^>]*>"#).expect("static regex is valid");
    img_tag_re.replace_all(content, "![image]($1)").to_string()
}

/// Compiles content to PDF using Pandoc and Typst.
///
/// All intermediate files (Markdown/HTML input, Typst sources, downloaded
/// images) and the final `<filename>.pdf` are written into `work_dir`, so
/// concurrent conversions can each use their own directory. Returns the path
/// of the produced PDF.
pub async fn compile_content_to_pdf(
    content: &str,
    filename: &str,
    preamble: &str,
    input_format: &str,
    work_dir: &Path,
    github_token: Option<&str>,
) -> Result<PathBuf> {
    fs::create_dir_all(work_dir)
        .await
        .with_context(|| format!("creating work dir {:?}", work_dir))?;

    let input_ext = if input_format == "html" { "html" } else { "md" };
    let input_path = work_dir.join(format!("{}.{}", filename, input_ext));
    let typ_path = work_dir.join(format!("{}.typ", filename));
    let final_typ_path = work_dir.join(format!("{}-final.typ", filename));
    let pdf_path = work_dir.join(format!("{}.pdf", filename));

    let processed_content = if input_format == "markdown" {
        preprocess_markdown(content)
    } else {
        content.to_string()
    };

    log::debug!("PDF: Writing {} content to {:?}", input_format, input_path);
    fs::write(&input_path, processed_content).await?;

    // Pandoc input -> typ
    log::debug!(
        "PDF: Running pandoc for {} (format: {})",
        filename,
        input_format
    );
    let format_arg = if input_format.contains("markdown") {
        // Disable citations to prevent @username from becoming a Typst #cite() command
        // which would cause compilation to fail without a bibliography.
        format!("{}-yaml_metadata_block-citations", input_format)
    } else {
        input_format.to_string()
    };

    let status = Command::new("pandoc")
        .args([
            "-f",
            &format_arg,
            &input_path.to_string_lossy(),
            "-o",
            &typ_path.to_string_lossy(),
        ])
        .status()
        .await;

    match status {
        Ok(s) if s.success() => {}
        _ => bail!("Pandoc conversion failed for format {}", input_format),
    }

    // Prepend preamble and fix link spacing
    log::debug!("PDF: Prepending preamble and fixing link spacing");
    let mut typ_content = fs::read_to_string(&typ_path).await?;

    // Fix link spacing in Typst.
    // Pandoc sometimes outputs "#link(url) [text]" which Typst renders with a space.
    // We want to ensure it is "#link(url)[text]".
    let re_link_args = Regex::new(r"#link\(([^)]+)\)\s+\[").expect("static regex is valid");
    typ_content = re_link_args
        .replace_all(&typ_content, "#link($1)[")
        .to_string();

    // Also remove leading spaces introduced by Pandoc before #link.
    let re_link_start = Regex::new(r"([^\w])\s+#link").expect("static regex is valid");
    typ_content = re_link_start
        .replace_all(&typ_content, "$1#link")
        .to_string();

    // Post-process Typst content to handle remote images and fix broken links.
    // Images are downloaded next to the Typst source so it can reference them
    // by plain filename.
    let processed_content = post_process_typst(&typ_content, work_dir, github_token).await?;

    // Make sure we have the mmdr import if we processed any mermaid blocks.
    let mmdr_import = "#import \"@preview/mmdr:0.2.2\": mermaid";
    let final_preamble = if !preamble.contains("mmdr") && processed_content.contains("#mermaid(") {
        format!("{}\n{}", mmdr_import, preamble)
    } else {
        preamble.to_string()
    };

    let final_content = format!("{}\n{}", final_preamble, processed_content);
    fs::write(&final_typ_path, &final_content).await?;

    // Typst compile
    log::debug!("PDF: Running typst compile for {}", filename);
    let status = Command::new("typst")
        .args([
            "compile",
            &final_typ_path.to_string_lossy(),
            &pdf_path.to_string_lossy(),
        ])
        .status()
        .await;

    match status {
        Ok(s) if s.success() => Ok(pdf_path),
        _ => bail!("Typst compilation failed"),
    }
}

/// Post-processes Typst content to fix Pandoc conversion artifacts and handle remote images.
///
/// `image_dir` – directory where remote images are downloaded and stored;
/// the Typst source is rewritten to reference them by filename, so the
/// compiler must run with its source in the same directory.
pub async fn post_process_typst(
    content: &str,
    image_dir: &Path,
    github_token: Option<&str>,
) -> Result<String> {
    let mut new_content = content.to_string();

    // 1. Replace page break markers with Typst #pagebreak()
    new_content = new_content.replace("PAGEBREAKPLACEHOLDER", "#pagebreak()");

    // 2. Fix broken automatic links merged with #link(...)
    // Regex matches a URL immediately followed by #link(...)
    // Example: https://github.com/foo#link("https://github.com/foo")
    // Note: Pandoc might escape # as \#, so we trim trailing backslashes.
    let link_re =
        Regex::new(r#"(https?://[^\s#]+)#link\("([^"]+)"\)"#).expect("static regex is valid");
    new_content = link_re
        .replace_all(&new_content, |caps: &regex::Captures| {
            let content = caps[1].trim_end_matches('\\');
            format!(r#"#link("{}")[{}]"#, &caps[2], content)
        })
        .to_string();

    // 3. Convert mermaid code blocks to mmdr plugin calls.
    // Pandoc outputs mermaid blocks in typst as: ```mermaid ... ```
    // Or if it contains backticks, it uses 4 or 5 backticks.
    for i in (3..=5).rev() {
        let backticks = "`".repeat(i);
        let pattern = format!(r"(?s){}mermaid\n(.*?)\n{}", backticks, backticks);
        if let Ok(re) = Regex::new(&pattern) {
            new_content = re
                .replace_all(&new_content, |caps: &regex::Captures| {
                    let inner = &caps[1];
                    format!("#mermaid({}\n{}\n{}.text)", backticks, inner, backticks)
                })
                .to_string();
        }
    }

    // 4. Download remote images and replace URLs with local paths
    // Regex matches Typst image commands: image("https://...") or #image("https://...")
    // We match the URL and allow for optional trailing arguments inside image(...)
    let img_re = Regex::new(r#"image\("([^"]+)"([^)]*)\)"#).expect("static regex is valid");
    // Regex matches Typst link commands: link("https://...") or #link("https://...")
    let link_cmd_re = Regex::new(r#"link\("([^"]+)"\)"#).expect("static regex is valid");

    let mut urls_to_download = HashSet::new();

    for caps in img_re.captures_iter(&new_content) {
        let url = &caps[1];
        if url.starts_with("http://") || url.starts_with("https://") {
            urls_to_download.insert(url.to_string());
        }
    }

    // Also scan links for potential artifacts (GitHub attachments, etc.)
    for caps in link_cmd_re.captures_iter(&new_content) {
        let url = &caps[1];
        if url.starts_with("http://") || url.starts_with("https://") {
            // Only download links that look like artifacts (files, assets, user-attachments)
            let is_artifact = url.contains("/files/")
                || url.contains("/assets/")
                || url.contains("/user-attachments/")
                || url.ends_with(".zip")
                || url.ends_with(".pdf")
                || url.ends_with(".gz")
                || url.ends_with(".tar");

            if is_artifact {
                log::debug!("PDF: Identified potential artifact link: {}", url);
                urls_to_download.insert(url.to_string());
            }
        }
    }

    if !urls_to_download.is_empty() {
        // We use reqwest's default behavior for redirects, which follows them
        // but drops the Authorization header when cross-domain (e.g. GitHub to S3).
        let client = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36")
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        // url → local filename for successfully resolved images.
        let mut url_to_local: HashMap<String, String> = HashMap::new();
        // Cache for release asset mappings: (owner, repo, tag) -> (filename -> asset_id)
        let mut release_cache: HashMap<(String, String, String), HashMap<String, u64>> =
            HashMap::new();

        let gh_release_re =
            Regex::new(r"https://github\.com/([^/]+)/([^/]+)/releases/download/([^/]+)/(.+)")
                .expect("static regex is valid");

        for url in urls_to_download {
            let local_filename = url_to_image_filename(&url);
            let local_path = image_dir.join(&local_filename);

            // Re-use an already-downloaded file without hitting the network.
            if local_path.exists() {
                log::debug!(
                    "PDF: Artifact already cached, skipping download: {:?}",
                    local_path
                );
                url_to_local.insert(url, local_filename);
                continue;
            }

            log::debug!("PDF: Downloading artifact {} to {:?}", url, local_path);

            let mut is_downloaded = false;

            // 1. Try the GitHub Release API if it's a release asset and we have a token
            if let (Some(caps), Some(token)) = (gh_release_re.captures(&url), github_token) {
                let owner = caps[1].to_string();
                let repo = caps[2].to_string();
                let tag = caps[3].to_string();
                let filename = caps[4].to_string();

                log::debug!(
                    "PDF: Attempting to download release asset via API: {}/{} tag {} file {}",
                    owner,
                    repo,
                    tag,
                    filename
                );

                let key = (owner.clone(), repo.clone(), tag.clone());
                if !release_cache.contains_key(&key) {
                    let release_url = format!(
                        "https://api.github.com/repos/{}/{}/releases/tags/{}",
                        owner, repo, tag
                    );
                    let release_resp = client.get(&release_url).bearer_auth(token).send().await;

                    if let Ok(resp) = release_resp {
                        if resp.status().is_success() {
                            if let Ok(json) = resp.json::<serde_json::Value>().await {
                                let mut asset_map = HashMap::new();
                                if let Some(assets) = json["assets"].as_array() {
                                    for asset in assets {
                                        if let (Some(name), Some(id)) =
                                            (asset["name"].as_str(), asset["id"].as_u64())
                                        {
                                            asset_map.insert(name.to_string(), id);
                                        }
                                    }
                                }
                                release_cache.insert(key.clone(), asset_map);
                            }
                        }
                    }
                }

                if let Some(asset_map) = release_cache.get(&key) {
                    if let Some(asset_id) = asset_map.get(&filename) {
                        let download_url = format!(
                            "https://api.github.com/repos/{}/{}/releases/assets/{}",
                            owner, repo, asset_id
                        );
                        let download_resp = client
                            .get(&download_url)
                            .header("Accept", "application/octet-stream")
                            .bearer_auth(token)
                            .send()
                            .await;

                        if let Ok(resp) = download_resp {
                            if resp.status().is_success() {
                                if let Ok(bytes) = resp.bytes().await {
                                    if let Err(e) = fs::write(&local_path, &bytes).await {
                                        log::error!(
                                            "PDF: Failed to save API downloaded asset {}: {}",
                                            filename,
                                            e
                                        );
                                    } else {
                                        log::debug!(
                                            "PDF: Successfully downloaded via API: {}",
                                            filename
                                        );
                                        url_to_local
                                            .insert(url.to_string(), local_filename.clone());
                                        is_downloaded = true;
                                    }
                                }
                            } else {
                                log::warn!(
                                    "PDF: API download failed for asset {}, status: {}",
                                    filename,
                                    resp.status()
                                );
                            }
                        }
                    }
                }
            }

            if is_downloaded {
                continue;
            }

            // 2. Regular HTTP download for everything else
            let mut req = client
                .get(&url)
                .header("Accept", "image/*, application/octet-stream, */*;q=0.8");

            if let Some(token) = github_token {
                if let Ok(parsed_url) = reqwest::Url::parse(&url) {
                    if let Some(host) = parsed_url.host_str() {
                        // Use token for GitHub domains
                        if host == "api.github.com"
                            || host == "github.com"
                            || host.ends_with(".githubusercontent.com")
                        {
                            req = req.bearer_auth(token);
                        }
                    }
                }
            }

            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    log::warn!("PDF: Request failed for artifact {}: {}", url, e);
                    continue;
                }
            };

            if resp.status().is_success() {
                if let Ok(bytes) = resp.bytes().await {
                    if let Err(e) = fs::write(&local_path, &bytes).await {
                        log::error!("PDF: Failed to save artifact {}: {}", local_filename, e);
                    } else {
                        // Store just the filename; the Typst source lives in the
                        // same directory, so a relative path resolves.
                        url_to_local.insert(url, local_filename);
                    }
                }
            } else {
                log::warn!(
                    "PDF: Failed to download artifact {}, status: {}, host: {:?}",
                    url,
                    resp.status(),
                    reqwest::Url::parse(&url)
                        .ok()
                        .and_then(|u| u.host_str().map(|s| s.to_string()))
                );
            }
        }

        // We need to parse images which might have a # prefix or not (depending on if they are in a figure or inline)
        // Pandoc outputs `#figure(image("..."))` or inline `#image(...)` or inside links `#link("...")[#image("...")]`
        // We will just match `image(...)` and leave whatever prefix was already there.
        new_content = img_re
            .replace_all(&new_content, |caps: &regex::Captures| {
                let url = &caps[1];
                let extra = &caps[2];
                if let Some(local_filename) = url_to_local.get(url) {
                    // Output only the local image reference without wrapping it in a link to GitHub.
                    format!(r#"image("{}"{})"#, local_filename, extra)
                } else if url.starts_with("http") {
                    log::warn!("PDF: Replacing failed image download with link for {}", url);
                    format!(r#"link("{}")[Image (Download Failed: {})]"#, url, url)
                } else {
                    format!(r#"image("{}"{})"#, url, extra)
                }
            })
            .to_string();

        new_content = link_cmd_re
            .replace_all(&new_content, |caps: &regex::Captures| {
                let url = &caps[1];
                if let Some(local_filename) = url_to_local.get(url) {
                    if !new_content.contains(&format!("image(\"{}\")", local_filename)) {
                        format!(r#"link("{}")"#, local_filename)
                    } else {
                        format!(r#"link("{}")"#, url)
                    }
                } else {
                    format!(r#"link("{}")"#, url)
                }
            })
            .to_string();
    }
    Ok(new_content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_preprocess_markdown() {
        let content = r#"Here is an image: <img src="https://example.com/test.png"> and another <IMG SRC='https://example.com/test2.png' width="100">"#;
        let processed = preprocess_markdown(content);
        assert!(processed.contains("![image](https://example.com/test.png)"));
        assert!(processed.contains("![image](https://example.com/test2.png)"));
    }

    #[test]
    fn test_slugify() {
        assert_eq!(slugify("Hello World!"), "hello-world");
        assert_eq!(slugify("2026-05-10: New Feature"), "2026-05-10-new-feature");
        assert_eq!(slugify("---Multiple---Dashes---"), "multiple-dashes");
    }

    fn comment(ts: &str, username: &str, body: &str, url: &str) -> UnifiedComment {
        UnifiedComment {
            id: 0,
            timestamp: DateTime::parse_from_rfc3339(ts)
                .unwrap()
                .with_timezone(&Utc),
            username: username.into(),
            body: body.into(),
            diff_hunk: None,
            url: url.into(),
        }
    }

    #[test]
    fn test_group_comments() {
        let comments = vec![
            comment("2026-05-10T10:00:00Z", "a", "b1", "u1"),
            // 1 min gap -> merged
            comment("2026-05-10T10:01:00Z", "a", "b2", "u2"),
            // 2 min 1 sec gap -> new group
            comment("2026-05-10T10:03:01Z", "a", "b3", "u3"),
            // 2 min gap -> merged
            comment("2026-05-10T10:05:01Z", "a", "b4", "u4"),
        ];

        let groups = group_comments(comments);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].len(), 2);
        assert_eq!(groups[1].len(), 2);
        assert_eq!(groups[0][0].body, "b1");
        assert_eq!(groups[0][1].body, "b2");
        assert_eq!(groups[1][0].body, "b3");
    }

    #[tokio::test]
    async fn test_post_process_typst_image_attributes() {
        let content = r#"#image("https://example.invalid/test.png", width: 100%)"#;
        let temp_dir = tempfile::tempdir().unwrap();

        // The download fails (invalid host), so the image is replaced with a link.
        let processed = post_process_typst(content, temp_dir.path(), None)
            .await
            .unwrap();

        assert!(processed.contains("Download Failed"));
        assert!(processed.contains("https://example.invalid/test.png"));
    }

    #[tokio::test]
    async fn test_post_process_typst_artifact_links() {
        let content = r#"#link("https://github.invalid/owner/repo/files/123/test.zip")"#;
        let temp_dir = tempfile::tempdir().unwrap();

        let processed = post_process_typst(content, temp_dir.path(), None)
            .await
            .unwrap();

        // It should still contain the link, even if download fails.
        assert!(
            processed.contains("link(\"https://github.invalid/owner/repo/files/123/test.zip\")")
        );
    }

    #[test]
    fn test_assemble_markdown() {
        let body = Some("Description here");

        let comments = vec![
            comment(
                "2026-05-10T10:00:00Z",
                "iesahin",
                "First comment",
                "https://github.com/example/repo/issues/1#issuecomment-1",
            ),
            UnifiedComment {
                diff_hunk: Some("@@ -1,1 +1,1 @@".to_string()),
                ..comment(
                    "2026-05-10T10:05:00Z",
                    "other",
                    "Second comment",
                    "https://github.com/example/repo/issues/1#issuecomment-2",
                )
            },
        ];

        let md = assemble_markdown(body, comments, "iesahin", None, 3);
        assert!(md.contains("Description here"));
        assert!(md.contains("PAGEBREAKPLACEHOLDER"));
        // Each comment is in its own group (5 min gap > 2 min threshold), so
        // both are the first item of their group and get a full date.
        assert!(md.contains(
            "**[2026-05-10 13:00](https://github.com/example/repo/issues/1#issuecomment-1)**"
        ));
        assert!(md.contains(
            "**[2026-05-10 13:05 - other](https://github.com/example/repo/issues/1#issuecomment-2)**"
        ));
        assert!(md.contains("```diff\n@@ -1,1 +1,1 @@\n```"));
        assert!(md.contains("First comment"));
        assert!(md.contains("Second comment"));

        // Verify description has a page break after it
        assert!(md.starts_with("Description here\n\nPAGEBREAKPLACEHOLDER\n\n"));

        // Groups are always ordered from most recent to oldest, so
        // "Second comment" appears BEFORE "First comment".
        let pos1 = md.find("First comment").unwrap();
        let pos2 = md.find("Second comment").unwrap();
        assert!(pos2 < pos1);
    }

    #[test]
    fn test_assemble_markdown_only_first_in_group_has_date() {
        let comments = vec![
            comment("2026-05-10T10:00:00Z", "iesahin", "First comment", "u1"),
            // 1 min gap -> same group
            comment("2026-05-10T10:01:00Z", "iesahin", "Second comment", "u2"),
        ];

        let md = assemble_markdown(None, comments, "iesahin", None, 3);
        // First comment of the (only) group gets the full date.
        assert!(md.contains("**[2026-05-10 13:00](u1)**"));
        // Second comment of the same group only gets the time.
        assert!(md.contains("**[13:01](u2)**"));
        assert!(!md.contains("2026-05-10 13:01"));
    }

    #[test]
    fn test_assemble_markdown_respects_timezone_option() {
        let comments = vec![comment("2026-05-10T10:00:00Z", "a", "body", "u1")];
        let md = assemble_markdown(None, comments, "", None, 0);
        assert!(md.contains("**[2026-05-10 10:00 - a](u1)**"));
    }

    #[test]
    fn test_assemble_markdown_includes_pr_diff() {
        let diff = PRDiff {
            base_ref: "main".into(),
            head_ref: "feature".into(),
            diff: "@@ -1 +1 @@".into(),
        };
        let md = assemble_markdown(Some("Body"), vec![], "", Some(diff), 3);
        assert!(md.contains("## PR Diff: main <- feature"));
        assert!(md.contains("```diff\n@@ -1 +1 @@\n```"));
    }

    #[tokio::test]
    async fn test_post_process_typst_page_breaks() {
        let content = "Some content\n\nPAGEBREAKPLACEHOLDER\n\nOther content";
        let temp_dir = tempfile::tempdir().unwrap();
        let processed = post_process_typst(content, temp_dir.path(), None)
            .await
            .unwrap();
        assert!(processed.contains("Some content\n\n#pagebreak()\n\nOther content"));
    }

    #[tokio::test]
    async fn test_post_process_typst_links() {
        let content = "Check this https://github.com/foo#link(\"https://github.com/foo\") and this https://bar\\#link(\"https://baz\")";
        let temp_dir = tempfile::tempdir().unwrap();
        let processed = post_process_typst(content, temp_dir.path(), None)
            .await
            .unwrap();
        assert!(processed.contains("#link(\"https://github.com/foo\")[https://github.com/foo]"));
        assert!(processed.contains("#link(\"https://baz\")[https://bar]"));
        assert!(!processed.contains("\\]"));
    }

    #[tokio::test]
    async fn test_post_process_typst_mermaid() {
        let content = "```mermaid\ngraph TD;\n  A-->B;\n```\n\n````mermaid\ngraph TD;\n  A[\"```code```\"]-->B;\n````";
        let temp_dir = tempfile::tempdir().unwrap();
        let processed = post_process_typst(content, temp_dir.path(), None)
            .await
            .unwrap();

        assert!(processed.contains("#mermaid(```\ngraph TD;\n  A-->B;\n```.text)"));
        assert!(
            processed.contains("#mermaid(````\ngraph TD;\n  A[\"```code```\"]-->B;\n````.text)")
        );
    }

    #[tokio::test]
    async fn test_build_preamble_uses_options() {
        let options = PdfOptions {
            paper: "us-letter".into(),
            font: "Test Font".into(),
            font_size_pt: 9,
            ..Default::default()
        };
        let preamble = build_preamble(&options, "My Title", "author", "date").await;
        assert!(preamble.contains(r#"paper: "us-letter""#));
        assert!(preamble.contains(r#"font: "Test Font", size: 9pt"#));
        assert!(preamble.contains("title: [My Title]"));
        assert!(preamble.contains(r#"authors: "author""#));
    }
}
