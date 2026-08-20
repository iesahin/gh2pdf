//! End-to-end checks that the Typst source gh2pdf generates really compiles.
//!
//! These need `pandoc` and `typst` on the PATH; when they are missing (as on
//! CI, which only runs the Rust toolchain) the tests report the skip and
//! pass, so the unit tests in `src/pdf.rs` remain the guard there.

use gh2pdf::config::PdfOptions;
use gh2pdf::models::PRDiff;
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

/// A PR diff has to reach the PDF whole. A diff of a Markdown file carries
/// that file's own fences, and a three-backtick block ends at the first of
/// them — the files below it would then be typeset as prose, without their
/// `+`/`-` markers.
#[tokio::test]
async fn pr_diffs_containing_code_fences_survive_the_pipeline() {
    if !tools_available() {
        eprintln!("skipping: pandoc/typst not installed");
        return;
    }

    let diff = concat!(
        "diff --git a/README.md b/README.md\n",
        "--- a/README.md\n",
        "+++ b/README.md\n",
        "@@ -1,4 +1,4 @@\n",
        " ```bash\n",
        "-old command\n",
        "+new command\n",
        " ```\n",
        "diff --git a/src/lib.rs b/src/lib.rs\n",
        "--- a/src/lib.rs\n",
        "+++ b/src/lib.rs\n",
        "@@ -1 +1 @@\n",
        "-pub fn a() {}\n",
        "+pub fn last_line_of_the_diff() {}"
    );
    let pr_diff = PRDiff {
        base_ref: "main".into(),
        head_ref: "feature".into(),
        diff: diff.to_string(),
    };
    let markdown = pdf::assemble_markdown(Some("Body."), vec![], "", Some(pr_diff), 0);

    let options = PdfOptions::default();
    let preamble = pdf::build_preamble(&options, "a pr", "2026-01-01 10:00", "d").await;
    let work_dir = std::env::temp_dir().join(format!("gh2pdf-test-diff-{}", std::process::id()));
    let name = "pr-diff";
    let result =
        pdf::compile_content_to_pdf(&markdown, name, &preamble, "markdown", &work_dir, None).await;
    assert!(matches!(&result, Ok(path) if path.exists()), "{:?}", result);

    let typst_source = std::fs::read_to_string(work_dir.join(format!("{}-final.typ", name)))
        .expect("the pipeline keeps its Typst source");
    let _ = std::fs::remove_dir_all(&work_dir);

    // Every line of the diff is still inside one raw block, markers included.
    let block_start = typst_source
        .find("````diff")
        .expect("the diff is fenced longer than the fences it contains");
    let block = &typst_source[block_start..];
    let block_end = block[8..].find("````").expect("the block is closed") + 8;
    let block = &block[..block_end];
    for line in diff.lines() {
        assert!(
            block.contains(line),
            "diff line missing from block: {}",
            line
        );
    }
}
