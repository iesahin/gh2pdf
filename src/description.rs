//! The `gh2pdf` block: a marker-delimited section in a GitHub issue or PR
//! description that records where the rendered PDF lives.
//!
//! ```text
//! <!-- gh2pdf:begin -->
//! 📄 [PDF](https://github.com/owner/repo/releases/download/gh2pdf/name.pdf) — updated 2026-07-26 18:19 UTC
//! <!-- gh2pdf:end -->
//! ```
//!
//! [`upsert_pdf_link`] writes the block idempotently; [`extract`] reads the
//! URL back out of it, for consumers that want to reuse an already rendered
//! PDF instead of compiling a new one.

use regex::Regex;

/// Opening marker of the block.
pub const BEGIN_MARKER: &str = "<!-- gh2pdf:begin -->";
/// Closing marker of the block.
pub const END_MARKER: &str = "<!-- gh2pdf:end -->";

/// Inserts or replaces the gh2pdf link block in `body`.
///
/// The block is delimited by HTML comment markers so repeated runs replace
/// the previous link instead of accumulating copies. A missing block is
/// appended to the end of the body.
pub fn upsert_pdf_link(body: &str, pdf_url: &str, updated_at: &str) -> String {
    let block = format!(
        "{}\n\u{1F4C4} [PDF]({}) — updated {}\n{}",
        BEGIN_MARKER, pdf_url, updated_at, END_MARKER
    );

    if let (Some(start), Some(end)) = (body.find(BEGIN_MARKER), body.find(END_MARKER)) {
        if start <= end {
            let after = end + END_MARKER.len();
            return format!("{}{}{}", &body[..start], block, &body[after..]);
        }
    }

    if body.trim().is_empty() {
        block
    } else {
        format!("{}\n\n{}", body.trim_end(), block)
    }
}

/// Returns the byte range of the `gh2pdf` block within `body`, markers
/// included, or `None` when the body carries no complete block.
fn block_span(body: &str) -> Option<std::ops::Range<usize>> {
    let start = body.find(BEGIN_MARKER)?;
    let end_start = body[start..].find(END_MARKER)? + start;
    Some(start..end_start + END_MARKER.len())
}

/// Extracts the PDF URL recorded in the `gh2pdf` block of an issue body.
/// Returns `None` when there is no block or it contains no URL.
///
/// The PDF is recorded as a Markdown link; a bare URL is accepted as a
/// fallback so a hand-edited block still resolves.
pub fn extract(body: &str) -> Option<String> {
    let markdown_link = Regex::new(r"\]\((https?://[^)\s]+)\)").expect("static regex is valid");
    let bare_url = Regex::new(r"https?://[^\s)\]]+").expect("static regex is valid");

    let inner = &body[block_span(body)?];
    markdown_link
        .captures(inner)
        .map(|c| c[1].to_string())
        .or_else(|| bare_url.find(inner).map(|m| m.as_str().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_appends_block_to_existing_body() {
        let body = "Some description";
        let out = upsert_pdf_link(body, "https://example.com/a.pdf", "2026-07-19 10:00 UTC");
        assert!(out.starts_with("Some description\n\n<!-- gh2pdf:begin -->"));
        assert!(out.contains("[PDF](https://example.com/a.pdf)"));
        assert!(out.ends_with(END_MARKER));
    }

    #[test]
    fn test_replaces_existing_block() {
        let body = "Intro\n\n<!-- gh2pdf:begin -->\nold link\n<!-- gh2pdf:end -->\n\nOutro";
        let out = upsert_pdf_link(body, "https://example.com/b.pdf", "2026-07-19 11:00 UTC");
        assert!(!out.contains("old link"));
        assert!(out.contains("[PDF](https://example.com/b.pdf)"));
        assert!(out.contains("Intro"));
        assert!(out.contains("Outro"));
        assert_eq!(out.matches(BEGIN_MARKER).count(), 1);
    }

    #[test]
    fn test_upsert_is_idempotent_for_same_inputs() {
        let once = upsert_pdf_link("Body", "https://x/y.pdf", "2026-07-19 12:00 UTC");
        let twice = upsert_pdf_link(&once, "https://x/y.pdf", "2026-07-19 12:00 UTC");
        assert_eq!(once, twice);
    }

    #[test]
    fn test_empty_body_gets_only_block() {
        let out = upsert_pdf_link("", "https://x/y.pdf", "2026-07-19 12:00 UTC");
        assert!(out.starts_with(BEGIN_MARKER));
        assert!(out.ends_with(END_MARKER));
    }

    const URL: &str =
        "https://github.com/iesahin/inboxbot/releases/download/gh2pdf/inboxbot-93-fix.pdf";

    /// The exact shape [`upsert_pdf_link`] writes.
    fn block(url: &str) -> String {
        format!(
            "{}\n📄 [PDF]({}) — updated 2026-07-26 18:19 UTC\n{}",
            BEGIN_MARKER, url, END_MARKER
        )
    }

    #[test]
    fn extract_reads_back_what_upsert_wrote() {
        let body = upsert_pdf_link("Some description.", URL, "2026-07-26 18:19 UTC");
        assert_eq!(extract(&body).as_deref(), Some(URL));
    }

    #[test]
    fn extract_reads_a_block_followed_by_more_text() {
        let body = format!("Description.\n\n{}\n\nTrailing note.", block(URL));
        assert_eq!(extract(&body).as_deref(), Some(URL));
    }

    #[test]
    fn extract_ignores_links_outside_the_block() {
        let body = "See [the docs](https://example.com/manual.pdf) for details.";
        assert_eq!(extract(body), None);
    }

    #[test]
    fn extract_returns_none_without_a_closing_marker() {
        let body = format!("{}\n📄 [PDF]({})", BEGIN_MARKER, URL);
        assert_eq!(extract(&body), None);
    }

    #[test]
    fn extract_returns_none_for_a_plain_description() {
        assert_eq!(extract("Just an ordinary issue body."), None);
        assert_eq!(extract(""), None);
    }

    #[test]
    fn extract_accepts_a_bare_url_in_the_block() {
        let body = format!("{}\n{}\n{}", BEGIN_MARKER, URL, END_MARKER);
        assert_eq!(extract(&body).as_deref(), Some(URL));
    }

    #[test]
    fn extract_returns_none_for_an_empty_block() {
        let body = format!("{}\n{}", BEGIN_MARKER, END_MARKER);
        assert_eq!(extract(&body), None);
    }
}
