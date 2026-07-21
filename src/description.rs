//! Idempotent management of the PDF link block inside an issue/PR description.

const BEGIN_MARKER: &str = "<!-- gh2pdf:begin -->";
const END_MARKER: &str = "<!-- gh2pdf:end -->";

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
}
