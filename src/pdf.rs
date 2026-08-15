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

/// Characters that carry markup meaning in a Typst content block and have to
/// be backslash-escaped when plain text is spliced into one.
const TYPST_MARKUP_SPECIALS: &[char] = &[
    '\\', '#', '[', ']', '*', '_', '`', '$', '<', '>', '@', '=', '-', '+', '/', '~',
];

/// Escapes plain text so Typst renders it literally inside a content block
/// (`[...]`).
///
/// Issue titles are arbitrary user text: a leading `#` starts a Typst code
/// expression, `@name` is a bibliography reference, `<name>` a label, and so
/// on — all of which abort the compilation instead of printing the character.
pub fn escape_typst_markup(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        if TYPST_MARKUP_SPECIALS.contains(&c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

/// Escapes plain text for use inside a Typst string literal (`"..."`).
pub fn escape_typst_string(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Derives a deterministic, filesystem-safe image filename from a remote URL.
///
/// The name is `<8-hex-hash>_<original-basename>`, where the hash covers the
/// full URL.  This guarantees:
/// - Same URL  → same filename on every run (idempotent, cache-friendly).
/// - Different URLs that happen to share a basename → different filenames
///   (no collision counter needed, no in-memory state required).
///
/// The query string and the `#fragment` are cut off the basename so the local
/// file keeps the extension of the actual image.
pub fn url_to_image_filename(url: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut hasher);
    let hash = hasher.finish();

    let basename = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty() && s.len() < 200)
        .unwrap_or("image.png");

    format!("{:08x}_{}", hash & 0xFFFF_FFFF, basename)
}

/// Upper bound on a slug's byte length. Slugs feed into filenames like
/// `<repo>-<number>-<slug>.pdf` or `<slug>-final.typ`; keeping the slug itself
/// well under the common 255-byte filesystem name limit leaves room for such
/// prefixes/suffixes even for long GitHub issue titles or web page titles.
const MAX_SLUG_LEN: usize = 80;

/// Generates a slug from a string (lowercase, replace non-alphanumeric with
/// hyphens), truncated to [`MAX_SLUG_LEN`] bytes at a word boundary so it
/// stays safe to use in filenames regardless of input length.
pub fn slugify(text: &str) -> String {
    let full = text
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");

    truncate_slug(&full, MAX_SLUG_LEN)
}

/// Truncates `slug` to at most `max_bytes` bytes without splitting a UTF-8
/// character, then backs up to the last `-` (if any) so the result doesn't
/// end mid-word.
fn truncate_slug(slug: &str, max_bytes: usize) -> String {
    if slug.len() <= max_bytes {
        return slug.to_string();
    }

    let mut end = 0;
    for (i, c) in slug.char_indices() {
        if i + c.len_utf8() > max_bytes {
            break;
        }
        end = i + c.len_utf8();
    }
    let cut = &slug[..end];

    match cut.rfind('-') {
        Some(idx) if idx > 0 => cut[..idx].to_string(),
        _ => cut.to_string(),
    }
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
/// `title_str` and `author_str` are plain text and are escaped for the
/// context the template puts them in (a content block for the title, a string
/// literal for the author), so titles like `# xvc as a file server` cannot
/// break the compilation. `date_str` is Typst markup produced by the caller
/// (the pipeline passes a `#link(...)`) and is inserted verbatim.
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
        .replace("TITLE_PLACEHOLDER", &escape_typst_markup(title_str))
        .replace("AUTHOR_PLACEHOLDER", &escape_typst_string(author_str))
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

/// Removes the spacing artefacts Pandoc leaves around `#link(...)` calls.
///
/// Pandoc sometimes writes `#link(url) [text]`, which Typst renders with a
/// space between the two; the argument list and the content block have to be
/// adjacent.
///
/// Whitespace *before* a `#link` is only spurious for HTML input, where the
/// source's own line breaks and indentation around `<a>` tags end up in the
/// Typst output. In Markdown such whitespace is a real word separator, so
/// removing it would glue the link to the preceding word.
pub fn fix_link_spacing(typ_content: &str, input_format: &str) -> String {
    let re_link_args = Regex::new(r"#link\(([^)]+)\)\s+\[").expect("static regex is valid");
    let fixed = re_link_args.replace_all(typ_content, "#link($1)[");

    if input_format == "html" {
        let re_link_start = Regex::new(r"([^\w])\s+#link").expect("static regex is valid");
        re_link_start.replace_all(&fixed, "$1#link").to_string()
    } else {
        fixed.to_string()
    }
}

/// How much of a failing tool's own output travels with the error. The first
/// diagnostics are the actionable ones (typst reports errors in source order),
/// and the whole message still has to fit in a chat message when a caller
/// relays it to whoever asked for the PDF.
const MAX_TOOL_ERROR_CHARS: usize = 1200;

/// Builds the error message for a failed external command, carrying the tool's
/// own diagnostics — typst's `error: …` lines, pandoc's parse errors — instead
/// of leaving them in the server's console: whoever asked for the PDF is the
/// one who needs to read them.
fn tool_failure_message(what: &str, exit_code: Option<i32>, stderr: &str, stdout: &str) -> String {
    let details = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    let code = match exit_code {
        Some(c) => format!(" (exit code {})", c),
        None => String::new(),
    };
    if details.is_empty() {
        return format!("{} failed{}", what, code);
    }

    let mut kept: String = details.chars().take(MAX_TOOL_ERROR_CHARS).collect();
    if kept.chars().count() < details.chars().count() {
        kept.push_str("\n… (output truncated)");
    }
    format!("{} failed{}:\n{}", what, code, kept)
}

/// Runs an external step of the PDF pipeline, turning a non-zero exit into an
/// error that quotes what the tool printed.
async fn run_pipeline_step(command: &mut Command, what: &str) -> Result<()> {
    let message = match command.output().await {
        Ok(output) if output.status.success() => return Ok(()),
        Ok(output) => tool_failure_message(
            what,
            output.status.code(),
            &String::from_utf8_lossy(&output.stderr),
            &String::from_utf8_lossy(&output.stdout),
        ),
        Err(e) => format!("{} could not be started: {}", what, e),
    };
    log::error!("PDF: {}", message);
    bail!(message)
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

    run_pipeline_step(
        Command::new("pandoc").args([
            "-f",
            &format_arg,
            &input_path.to_string_lossy(),
            "-o",
            &typ_path.to_string_lossy(),
        ]),
        &format!("Pandoc conversion of format {}", input_format),
    )
    .await?;

    // Prepend preamble and fix link spacing
    log::debug!("PDF: Prepending preamble and fixing link spacing");
    let typ_content = fix_link_spacing(&fs::read_to_string(&typ_path).await?, input_format);

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
    run_pipeline_step(
        Command::new("typst").args([
            "compile",
            &final_typ_path.to_string_lossy(),
            &pdf_path.to_string_lossy(),
        ]),
        "Typst compilation",
    )
    .await?;

    Ok(pdf_path)
}

/// Returns the body of the Typst content block that starts at `start`, plus
/// the index just past its closing `]`. `None` when `start` is not a `[` or
/// the block is unterminated.
fn content_block_at(content: &str, start: usize) -> Option<(&str, usize)> {
    let bytes = content.as_bytes();
    if bytes.get(start) != Some(&b'[') {
        return None;
    }

    // `[`, `]` and `\` are ASCII, so byte scanning never splits a character.
    let mut depth = 0usize;
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some((&content[start + 1..i], i + 1));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Collects the labels the document actually defines.
///
/// Pandoc always writes a label at the end of a line — on its own after a
/// heading, or after the `]`/`)` of a figure or block. Raw blocks are skipped:
/// a `<html>` line inside a fenced code block is printed text, not a label,
/// and mistaking it for one would keep a link that Typst then rejects.
fn defined_labels(content: &str) -> HashSet<&str> {
    let label_re = Regex::new(r"<([^<>\s]+)>$").expect("static regex is valid");
    let mut labels = HashSet::new();
    let mut in_raw_block = false;

    for line in content.lines() {
        let line = line.trim_end();
        if line.trim_start().starts_with("```") || line.trim_start().starts_with("~~~") {
            in_raw_block = !in_raw_block;
            continue;
        }
        if in_raw_block {
            continue;
        }
        if let Some(caps) = label_re.captures(line) {
            let whole = caps.get(0).expect("group 0 always matches");
            // `#link(<name>)` is a reference, not a definition.
            if line[..whole.start()].ends_with('(') {
                continue;
            }
            if let Some(name) = caps.get(1) {
                labels.insert(name.as_str());
            }
        }
    }
    labels
}

/// Replaces links to undefined labels by their link text.
///
/// Pandoc turns a Markdown anchor link (`[text](#anchor)`) into a Typst label
/// reference (`#link(<anchor>)[text]`). Typst aborts the compilation when the
/// label is not defined in the document, which is the common case for issue
/// bodies: they link to anchors of a README, of the rendered issue page, or
/// of a heading that only exists in another comment. Anchors that *do* have a
/// matching heading in the document are left alone so they stay clickable.
fn resolve_label_links(content: &str) -> String {
    let defined = defined_labels(content);

    let link_re = Regex::new(r"#link\(<([^<>\s]+)>\)").expect("static regex is valid");
    let mut resolved = String::with_capacity(content.len());
    let mut copied_to = 0usize;

    for caps in link_re.captures_iter(content) {
        let whole = caps.get(0).expect("group 0 always matches");
        let name = match caps.get(1) {
            Some(m) => m.as_str(),
            None => continue,
        };
        // Skip matches inside the body of an already-rewritten link.
        if whole.start() < copied_to || defined.contains(name) {
            continue;
        }

        resolved.push_str(&content[copied_to..whole.start()]);
        match content_block_at(content, whole.end()) {
            // `#[body]` keeps the text and stays an expression, so a `;`
            // terminator Pandoc may have written after the block still binds
            // to something.
            Some((body, end)) => {
                log::debug!("PDF: Dropping link to undefined anchor <{}>", name);
                resolved.push_str("#[");
                resolved.push_str(body);
                resolved.push(']');
                copied_to = end;
            }
            None => {
                log::debug!("PDF: Dropping bare link to undefined anchor <{}>", name);
                resolved.push_str("#[");
                resolved.push_str(&escape_typst_markup(&format!("#{}", name)));
                resolved.push(']');
                copied_to = whole.end();
            }
        }
    }

    resolved.push_str(&content[copied_to..]);
    resolved
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

    // 3. Defuse links to anchors that do not exist in this document.
    new_content = resolve_label_links(&new_content);

    // 4. Convert mermaid code blocks to mmdr plugin calls.
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

    // 5. Download remote images and replace URLs with local paths
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

            // 2. Fall back to the `gh` CLI for release assets the API call
            //    could not fetch (a token scoped to another repository, or no
            //    token at all while the user is logged in to `gh`). A missing
            //    `gh` just fails the command and we move on to plain HTTP.
            if let Some(caps) = gh_release_re.captures(&url) {
                let owner = &caps[1];
                let repo = &caps[2];
                let tag = &caps[3];
                let filename = &caps[4];

                log::debug!(
                    "PDF: Falling back to gh cli for release asset: {}/{} tag {} file {}",
                    owner,
                    repo,
                    tag,
                    filename
                );

                let gh_status = Command::new("gh")
                    .args([
                        "release",
                        "download",
                        tag,
                        "-p",
                        filename,
                        "--dir",
                        &image_dir.to_string_lossy(),
                        "--repo",
                        &format!("{}/{}", owner, repo),
                        "--clobber",
                    ])
                    .status()
                    .await;

                if let Ok(status) = gh_status {
                    if status.success() {
                        log::debug!("PDF: Successfully downloaded using gh cli: {}", filename);

                        let downloaded_file = image_dir.join(filename);
                        if downloaded_file.exists() {
                            if let Err(e) = fs::rename(&downloaded_file, &local_path).await {
                                log::warn!(
                                    "PDF: Could not rename gh downloaded file {:?} to {:?}: {}",
                                    downloaded_file,
                                    local_path,
                                    e
                                );
                            } else {
                                url_to_local.insert(url.to_string(), local_filename.clone());
                                is_downloaded = true;
                            }
                        }
                    }
                }
            }

            if is_downloaded {
                continue;
            }

            // 3. Regular HTTP download for everything else
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
                    // The URL is shown as text, so it has to be escaped: a
                    // `#fragment` would otherwise start a Typst expression.
                    format!(
                        r#"link("{}")[Image (Download Failed: {})]"#,
                        url,
                        escape_typst_markup(url)
                    )
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
    fn test_escape_typst_markup() {
        // The reported failure: a title starting with a Markdown heading
        // marker turned into a Typst code expression.
        assert_eq!(
            escape_typst_markup("# xvc as a file server"),
            "\\# xvc as a file server"
        );
        assert_eq!(
            escape_typst_markup("Fix [bug] in *foo* @user <tag> a_b $x$ 2-3 c/d ~e `f` +g =h \\i"),
            "Fix \\[bug\\] in \\*foo\\* \\@user \\<tag\\> a\\_b \\$x\\$ 2\\-3 c\\/d \\~e \\`f\\` \
             \\+g \\=h \\\\i"
        );
        assert_eq!(escape_typst_markup("plain title 123"), "plain title 123");
    }

    #[test]
    fn test_escape_typst_string() {
        assert_eq!(
            escape_typst_string(r#"say "hi" \ there"#),
            r#"say \"hi\" \\ there"#
        );
    }

    #[test]
    fn test_url_to_image_filename_strips_query_and_fragment() {
        let name = url_to_image_filename("https://example.com/a/pic.png?jwt=abc#anchor");
        assert!(name.ends_with("_pic.png"), "unexpected filename: {}", name);
    }

    #[test]
    fn test_fix_link_spacing() {
        // The argument list and the content block must be adjacent.
        assert_eq!(
            fix_link_spacing(r#"#link("https://x") [text]"#, "markdown"),
            r#"#link("https://x")[text]"#
        );
        // Markdown: the space before the link separates words and stays.
        assert_eq!(
            fix_link_spacing(r#"Md link: #link("https://x")[text]"#, "markdown"),
            r#"Md link: #link("https://x")[text]"#
        );
        // HTML: the whitespace comes from the source's formatting.
        assert_eq!(
            fix_link_spacing("Md link:\n#link(\"https://x\")[text]", "html"),
            r#"Md link:#link("https://x")[text]"#
        );
    }

    #[test]
    fn test_slugify() {
        assert_eq!(slugify("Hello World!"), "hello-world");
        assert_eq!(slugify("2026-05-10: New Feature"), "2026-05-10-new-feature");
        assert_eq!(slugify("---Multiple---Dashes---"), "multiple-dashes");
    }

    #[test]
    fn test_slugify_truncates_long_titles() {
        let title = "word ".repeat(100); // far past MAX_SLUG_LEN once slugified
        let slug = slugify(&title);
        assert!(
            slug.len() <= MAX_SLUG_LEN,
            "slug of length {} exceeds MAX_SLUG_LEN",
            slug.len()
        );
        assert!(!slug.ends_with('-'));
        assert!(slug.starts_with("word-word"));
    }

    #[test]
    fn test_slugify_truncates_without_splitting_utf8_chars() {
        // Turkish text with multi-byte characters right at the truncation boundary.
        let title = "çğıöşü ".repeat(30);
        let slug = slugify(&title); // must not panic on a mid-character split
        assert!(slug.len() <= MAX_SLUG_LEN);
        assert!(slug.is_char_boundary(slug.len()));
    }

    /// A bare "Typst compilation failed" told the user nothing about what in
    /// the document typst choked on; the tool's own diagnostics travel with
    /// the error now.
    #[test]
    fn tool_failure_message_quotes_the_tools_diagnostics() {
        let message = tool_failure_message(
            "Typst compilation",
            Some(1),
            "error: unknown variable: mermaid\n  ┌─ /tmp/a-final.typ:12:2",
            "",
        );

        assert!(message.starts_with("Typst compilation failed (exit code 1):"));
        assert!(message.contains("error: unknown variable: mermaid"));
    }

    #[test]
    fn tool_failure_message_falls_back_to_stdout_and_truncates() {
        let noise = "e".repeat(MAX_TOOL_ERROR_CHARS + 50);
        let message = tool_failure_message("Pandoc conversion", None, "   ", &noise);

        assert!(message.starts_with("Pandoc conversion failed:"));
        assert!(message.ends_with("… (output truncated)"));
        assert!(message.len() < noise.len() + 100);
    }

    #[test]
    fn tool_failure_message_without_output_still_names_the_step() {
        assert_eq!(
            tool_failure_message("Typst compilation", None, "", ""),
            "Typst compilation failed"
        );
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
    async fn test_post_process_typst_drops_undefined_anchor_links() {
        // Pandoc renders `[text](#anchor)` as a label reference; Typst fails
        // to compile when the label is not in the document.
        let content = "= Install Now\n<install-now>\n\nGo to #link(<install-now>)[install] or \
                       #link(<nope>)[missing];.\n\nBare #link(<gone>)\n";
        let temp_dir = tempfile::tempdir().unwrap();
        let processed = post_process_typst(content, temp_dir.path(), None)
            .await
            .unwrap();

        // The anchor that has a heading keeps its link.
        assert!(processed.contains("#link(<install-now>)[install]"));
        // The dangling ones keep their text but lose the reference.
        assert!(processed.contains("or #[missing];."));
        assert!(processed.contains("Bare #[\\#gone]"));
        assert!(!processed.contains("<nope>"));
        assert!(!processed.contains("<gone>"));
    }

    #[tokio::test]
    async fn test_post_process_typst_ignores_labels_inside_code_blocks() {
        // `<html>` in a code block is printed text, not a label, so the link
        // to `#html` is still dangling.
        let content = "```xml\n<html>\n```\n\nSee #link(<html>)[b]\n";
        let temp_dir = tempfile::tempdir().unwrap();
        let processed = post_process_typst(content, temp_dir.path(), None)
            .await
            .unwrap();
        assert!(processed.contains("See #[b]"));
        assert!(processed.contains("```xml\n<html>\n```"));
    }

    #[tokio::test]
    async fn test_post_process_typst_keeps_nested_brackets_of_anchor_links() {
        let content = "#link(<nope>)[text with [nested] brackets]";
        let temp_dir = tempfile::tempdir().unwrap();
        let processed = post_process_typst(content, temp_dir.path(), None)
            .await
            .unwrap();
        assert_eq!(processed, "#[text with [nested] brackets]");
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

    #[tokio::test]
    async fn test_build_preamble_escapes_title_and_author() {
        let options = PdfOptions::default();
        let preamble = build_preamble(
            &options,
            "# xvc as a file server",
            r#"feature/"quoted""#,
            "#link(\"https://example.com\")[repo\\#1]",
        )
        .await;

        assert!(preamble.contains("title: [\\# xvc as a file server]"));
        assert!(preamble.contains(r#"authors: "feature/\"quoted\"""#));
        // The date field is Typst markup from the caller and stays verbatim.
        assert!(preamble.contains("date: [#link(\"https://example.com\")[repo\\#1]]"));
    }
}
