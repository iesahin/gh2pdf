//! End-to-end checks that the Typst source gh2pdf generates really compiles.
//!
//! These need `pandoc` and `typst` on the PATH; when they are missing (as on
//! CI, which only runs the Rust toolchain) the tests report the skip and
//! pass, so the unit tests in `src/pdf.rs` remain the guard there.

use gh2pdf::config::PdfOptions;
use gh2pdf::pdf;

fn tools_available() -> bool {
    ["pandoc", "typst"].iter().all(|tool| {
        std::process::Command::new(tool)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// Renders `title`/`body` the way the pipeline does and returns whether the
/// PDF was produced.
async fn renders(title: &str, body: &str, name: &str) -> bool {
    let options = PdfOptions::default();
    let date = format!(
        "#link(\"https://github.com/o/r/issues/550\")[{}\\#550]",
        pdf::escape_typst_markup("xvc_repo")
    );
    let preamble = pdf::build_preamble(&options, title, "2026-01-01 10:00", &date).await;
    let markdown = pdf::assemble_markdown(Some(body), vec![], "", None, 0);

    let work_dir =
        std::env::temp_dir().join(format!("gh2pdf-test-{}-{}", name, std::process::id()));
    let result =
        pdf::compile_content_to_pdf(&markdown, name, &preamble, "markdown", &work_dir, None).await;
    let rendered = matches!(&result, Ok(path) if path.exists());
    let _ = std::fs::remove_dir_all(&work_dir);
    rendered
}

#[tokio::test]
async fn titles_with_markup_characters_compile() {
    if !tools_available() {
        eprintln!("skipping: pandoc/typst not installed");
        return;
    }

    assert!(renders("# xvc as a file server", "Body.", "title-heading").await);
    assert!(
        renders(
            "Fix [bug] in *foo* #12 $x$ @user <tag> 100% a_b",
            "Body.",
            "title-specials"
        )
        .await
    );
}

#[tokio::test]
async fn urls_with_fragments_compile() {
    if !tools_available() {
        eprintln!("skipping: pandoc/typst not installed");
        return;
    }

    let body = r#"Bare https://github.com/iesahin/xvc/issues/550#issuecomment-1 in text.

Autolink: <https://github.com/a/b#frag>

Inline: [see this](https://github.com/a/b#frag-2)

Image: ![i](https://example.invalid/i.png#anchor)

Dangling anchor: [installation](#installation)

```xml
<installation>
```
"#;

    assert!(renders("normal title", body, "urls-fragments").await);
}
