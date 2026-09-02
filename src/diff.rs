//! Splitting a pull request's unified diff into its files, and rendering each
//! file so the PDF reader can jump from a line straight to the same line on
//! GitHub.
//!
//! The PDF gives every changed file a page of its own and every diff line a
//! link into the pull request's *Files changed* view. GitHub anchors a file
//! there as `#diff-<sha256 of the file path>` and a line within it as that
//! anchor plus `L<n>` (left/old side) or `R<n>` (right/new side), so the link
//! lands on the exact line — where the review-comment button sits.
//!
//! Lines are rendered as a Typst grid rather than a code block, because a
//! code block is verbatim: nothing inside it can be a link. The grid keeps
//! the old/new line numbers in a gutter, colours the rows the way GitHub
//! does, and wraps the whole row in the link.

use crate::models::PRDiff;
use crate::pdf::escape_typst_string;
use sha2::{Digest, Sha256};

/// What a line of a unified diff says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// `diff --git`, `index`, `--- a/x`, `+++ b/y`, mode and rename lines:
    /// everything before the first hunk.
    Header,
    /// A `@@ -a,b +c,d @@` hunk header.
    Hunk,
    /// An unchanged line, present on both sides.
    Context,
    /// A line only the new file has.
    Added,
    /// A line only the old file has.
    Removed,
    /// `\ No newline at end of file` — part of neither side.
    Marker,
}

/// One line of a file's diff, with the line numbers it carries on each side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    /// Line number in the old file, for lines the old file has.
    pub old_line: Option<u32>,
    /// Line number in the new file, for lines the new file has.
    pub new_line: Option<u32>,
    /// The line as it appears in the diff, `+`/`-`/space prefix included.
    pub text: String,
}

/// The diff of a single file, as split out of a pull request's diff.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileDiff {
    /// Path on the old side; `None` when the file is being added.
    pub old_path: Option<String>,
    /// Path on the new side; `None` when the file is being deleted.
    pub new_path: Option<String>,
    pub lines: Vec<DiffLine>,
}

impl FileDiff {
    /// The path GitHub anchors this file's diff under: the new path, or the
    /// old one when the file was deleted.
    pub fn path(&self) -> &str {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .unwrap_or("")
    }

    /// How many lines the file gains.
    pub fn added(&self) -> usize {
        self.lines
            .iter()
            .filter(|l| l.kind == LineKind::Added)
            .count()
    }

    /// How many lines the file loses.
    pub fn removed(&self) -> usize {
        self.lines
            .iter()
            .filter(|l| l.kind == LineKind::Removed)
            .count()
    }

    /// Whether the file moved, so the heading can show both paths.
    pub fn is_rename(&self) -> bool {
        match (&self.old_path, &self.new_path) {
            (Some(old), Some(new)) => old != new,
            _ => false,
        }
    }
}

/// The *Files changed* page of pull request `number` in `owner/repo`, which
/// is what [`PRDiff::files_url`](crate::models::PRDiff::files_url) wants.
pub fn pr_files_url(owner: &str, repo: &str, number: u64) -> String {
    format!(
        "https://github.com/{}/{}/pull/{}/files",
        owner, repo, number
    )
}

/// GitHub's anchor for a file's diff: `diff-` followed by the hex SHA-256 of
/// the file's path.
pub fn file_anchor(path: &str) -> String {
    let digest = Sha256::digest(path.as_bytes());
    let mut anchor = String::with_capacity(5 + digest.len() * 2);
    anchor.push_str("diff-");
    for byte in digest {
        anchor.push_str(&format!("{:02x}", byte));
    }
    anchor
}

/// Splits a unified diff into one [`FileDiff`] per file.
///
/// Content that precedes the first `diff --git` header (nothing, normally)
/// becomes a leading entry with no paths, so no part of the diff is dropped.
pub fn split_diff(diff: &str) -> Vec<FileDiff> {
    let mut files: Vec<Vec<&str>> = Vec::new();
    for line in diff.lines().map(|l| l.strip_suffix('\r').unwrap_or(l)) {
        if line.starts_with("diff --git ") || files.is_empty() {
            files.push(Vec::new());
        }
        files
            .last_mut()
            .expect("a chunk was just pushed")
            .push(line);
    }

    files
        .into_iter()
        .filter(|chunk| chunk.iter().any(|l| !l.trim().is_empty()))
        .map(|chunk| parse_file_chunk(&chunk))
        .collect()
}

/// Parses one file's lines out of a diff chunk, numbering them from the hunk
/// headers.
fn parse_file_chunk(chunk: &[&str]) -> FileDiff {
    let mut file = FileDiff::default();
    let (mut old_no, mut new_no) = (0u32, 0u32);
    let mut in_hunk = false;

    for line in chunk {
        if let Some((old_start, new_start)) = parse_hunk_header(line) {
            in_hunk = true;
            old_no = old_start;
            new_no = new_start;
            file.lines.push(DiffLine {
                kind: LineKind::Hunk,
                old_line: None,
                new_line: None,
                text: (*line).to_string(),
            });
            continue;
        }

        if !in_hunk {
            if let Some(path) = line.strip_prefix("--- ") {
                file.old_path = header_path(path);
            } else if let Some(path) = line.strip_prefix("+++ ") {
                file.new_path = header_path(path);
            } else if let Some(rest) = line.strip_prefix("diff --git ") {
                // Only used when the chunk carries no `---`/`+++` pair, as
                // for a binary file.
                let (old, new) = git_header_paths(rest);
                file.old_path = file.old_path.take().or(old);
                file.new_path = file.new_path.take().or(new);
            }
            file.lines.push(DiffLine {
                kind: LineKind::Header,
                old_line: None,
                new_line: None,
                text: (*line).to_string(),
            });
            continue;
        }

        let (kind, old_line, new_line) = match line.chars().next() {
            Some('+') => {
                new_no += 1;
                (LineKind::Added, None, Some(new_no))
            }
            Some('-') => {
                old_no += 1;
                (LineKind::Removed, Some(old_no), None)
            }
            Some('\\') => (LineKind::Marker, None, None),
            // A context line is " text"; an empty line in the file reaches us
            // as an empty diff line, since trailing whitespace is often
            // stripped on the way.
            _ => {
                old_no += 1;
                new_no += 1;
                (LineKind::Context, Some(old_no), Some(new_no))
            }
        };

        file.lines.push(DiffLine {
            kind,
            old_line,
            new_line,
            text: (*line).to_string(),
        });
    }

    file
}

/// Reads the start line of each side out of a `@@ -a,b +c,d @@` header.
fn parse_hunk_header(line: &str) -> Option<(u32, u32)> {
    let rest = line.strip_prefix("@@ ")?;
    let end = rest.find(" @@")?;
    let mut ranges = rest[..end].split_whitespace();
    let old = ranges.next()?.strip_prefix('-')?;
    let new = ranges.next()?.strip_prefix('+')?;
    let start = |range: &str| -> Option<u32> {
        range
            .split(',')
            .next()
            .and_then(|n| n.parse::<u32>().ok())
            .map(|n| n.saturating_sub(1))
    };
    Some((start(old)?, start(new)?))
}

/// Reads the path out of a `--- a/path` or `+++ b/path` header, returning
/// `None` for the `/dev/null` side of an added or deleted file.
fn header_path(field: &str) -> Option<String> {
    // Git may append a tab and a timestamp to the path.
    let field = field.split('\t').next().unwrap_or(field).trim_end();
    let path = unquote_path(field);
    if path == "/dev/null" {
        return None;
    }
    Some(strip_side_prefix(&path))
}

/// Reads both paths out of a `diff --git a/old b/new` header.
///
/// The two paths are separated by a space, which a path may itself contain;
/// the split is therefore made at the ` b/` that leaves an `a/`-prefixed
/// first half, falling back to the last space.
fn git_header_paths(rest: &str) -> (Option<String>, Option<String>) {
    if rest.starts_with('"') {
        // Quoted paths are separated by `" "`.
        if let Some(split) = rest.find("\" \"") {
            let old = unquote_path(&rest[..split + 1]);
            let new = unquote_path(&rest[split + 2..]);
            return (Some(strip_side_prefix(&old)), Some(strip_side_prefix(&new)));
        }
    }
    let split = rest
        .match_indices(" b/")
        .find(|(i, _)| rest[..*i].starts_with("a/"))
        .map(|(i, _)| i)
        .or_else(|| rest.rfind(' '));
    match split {
        Some(i) => (
            Some(strip_side_prefix(&rest[..i])),
            Some(strip_side_prefix(&rest[i + 1..])),
        ),
        None => (None, None),
    }
}

/// Drops the `a/` or `b/` side prefix git puts in front of a diff path.
fn strip_side_prefix(path: &str) -> String {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
        .to_string()
}

/// Undoes git's C-style quoting of paths that carry non-ASCII or control
/// characters (`"src/\303\251t\303\251.rs"`), so the SHA-256 anchor is taken
/// over the real path. Anything that does not decode is returned unchanged.
fn unquote_path(field: &str) -> String {
    let Some(inner) = field
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    else {
        return field.to_string();
    };

    let mut bytes = Vec::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('n') => bytes.push(b'\n'),
            Some('t') => bytes.push(b'\t'),
            Some('r') => bytes.push(b'\r'),
            Some('"') => bytes.push(b'"'),
            Some('\\') => bytes.push(b'\\'),
            Some(d @ '0'..='7') => {
                let mut octal = d.to_string();
                for _ in 0..2 {
                    match chars.clone().next() {
                        Some(n @ '0'..='7') => {
                            octal.push(n);
                            chars.next();
                        }
                        _ => break,
                    }
                }
                match u8::from_str_radix(&octal, 8) {
                    Ok(byte) => bytes.push(byte),
                    Err(_) => return field.to_string(),
                }
            }
            _ => return field.to_string(),
        }
    }

    String::from_utf8(bytes).unwrap_or_else(|_| field.to_string())
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Point size the diff is set in. Small enough that a 100-column line fits
/// the widened diff page without wrapping.
const DIFF_FONT_SIZE_PT: f32 = 7.5;

/// GitHub's own row colours, so a reader who knows the web view recognises
/// the page.
const ADDED_FILL: &str = "#e6ffec";
const REMOVED_FILL: &str = "#ffebe9";
const HUNK_FILL: &str = "#ddf4ff";
const HUNK_COLOR: &str = "#0550ae";

/// Narrows the page margins for the diff pages, which start after this is
/// emitted and run to the end of the document.
const WIDE_PAGE: &str = "```{=typst}\n#set page(margin: 1.4cm)\n```\n\n";

/// Renders a pull request's diff as Markdown: an index of the changed files,
/// then one page per file whose every line links to the same line in the
/// pull request's *Files changed* view.
///
/// The per-line links need [`PRDiff::files_url`]; without it the same
/// document is produced with plain, unlinked lines.
pub fn render_pr_diff(pr: &PRDiff) -> String {
    let files = split_diff(&pr.diff);
    if files.is_empty() {
        return String::new();
    }

    let mut md = format!("## PR Diff: {} <- {}\n\n", pr.base_ref, pr.head_ref);
    for file in &files {
        md.push_str(&index_entry(file, pr.files_url.as_deref()));
    }

    for (i, file) in files.iter().enumerate() {
        md.push_str("\nPAGEBREAKPLACEHOLDER\n\n");
        if i == 0 {
            // The diff wants every millimetre of the page, and the wide
            // column a Tufte-style template keeps for sidenotes holds none
            // of it. Setting this on the empty page the break just opened
            // leaves the rest of the document untouched.
            md.push_str(WIDE_PAGE);
        }
        md.push_str(&file_heading(file, pr.files_url.as_deref()));
        md.push_str(&render_file_typst(file, pr.files_url.as_deref()));
    }

    md
}

/// One line of the diff index: the file, linked to its diff on GitHub, with
/// how many lines it gains and loses.
fn index_entry(file: &FileDiff, files_url: Option<&str>) -> String {
    let label = if file.is_rename() {
        format!(
            "`{}` → `{}`",
            file.old_path.as_deref().unwrap_or(""),
            file.path()
        )
    } else {
        format!("`{}`", file.path())
    };
    let linked = match files_url {
        Some(url) if !file.path().is_empty() => {
            format!("[{}]({}#{})", label, url, file_anchor(file.path()))
        }
        _ => label,
    };
    format!("- {} (+{}, -{})\n", linked, file.added(), file.removed())
}

/// The heading a file's diff page carries: its path, linked to that file's
/// diff on GitHub.
fn file_heading(file: &FileDiff, files_url: Option<&str>) -> String {
    let path = file.path();
    let title = if file.is_rename() {
        format!("`{}` → `{}`", file.old_path.as_deref().unwrap_or(""), path)
    } else if path.is_empty() {
        "`(diff)`".to_string()
    } else {
        format!("`{}`", path)
    };
    match files_url {
        Some(url) if !path.is_empty() => {
            format!("### [{}]({}#{})\n\n", title, url, file_anchor(path))
        }
        _ => format!("### {}\n\n", title),
    }
}

/// Renders one file's diff as a raw Typst block: a three-column grid of the
/// old line number, the new line number and the line itself, each row
/// coloured by what the line does and linked to that line on GitHub.
///
/// A code block cannot carry links — its content is verbatim — which is why
/// the diff is built as Typst rather than left to Pandoc.
fn render_file_typst(file: &FileDiff, files_url: Option<&str>) -> String {
    if file.lines.is_empty() {
        return String::new();
    }

    let anchor = file_anchor(file.path());
    let link_base = files_url.filter(|_| !file.path().is_empty());

    let mut fills = String::new();
    let mut rows = String::new();
    for line in file.lines.iter() {
        // The trailing comma matters: `(n)` is a parenthesised value in
        // Typst, only `(n,)` is a one-element array.
        fills.push_str(match line.kind {
            LineKind::Added => "a, ",
            LineKind::Removed => "d, ",
            LineKind::Hunk => "h, ",
            _ => "n, ",
        });

        let target = link_base.map(|url| match (line.kind, line.old_line, line.new_line) {
            (LineKind::Removed, Some(old), _) => format!("{}#{}L{}", url, anchor, old),
            (_, _, Some(new)) => format!("{}#{}R{}", url, anchor, new),
            _ => format!("{}#{}", url, anchor),
        });

        rows.push_str(&gutter_cell(line.old_line, target.as_deref()));
        rows.push_str(&gutter_cell(line.new_line, target.as_deref()));
        rows.push_str(&text_cell(line, target.as_deref()));
        rows.push('\n');
    }

    format!(
        r#"```{{=typst}}
#block(width: 100%, breakable: true)[
#let a = rgb("{}")
#let d = rgb("{}")
#let h = rgb("{}")
#let n = none
#let fills = ({})
#set text(font: ("DejaVu Sans Mono", "Liberation Mono", "Courier New"), size: {}pt)
#set par(justify: false, leading: 0.35em)
#grid(
columns: (auto, auto, 1fr),
inset: (x: 2pt, y: 0.9pt),
align: (right + top, right + top, left + top),
fill: (col, row) => fills.at(row, default: none),
{})
]
```

"#,
        ADDED_FILL, REMOVED_FILL, HUNK_FILL, fills, DIFF_FONT_SIZE_PT, rows
    )
}

/// A line-number cell: the number, linked, or an empty cell for the side the
/// line does not belong to.
fn gutter_cell(number: Option<u32>, target: Option<&str>) -> String {
    match (number, target) {
        (None, _) => "[],".to_string(),
        (Some(n), Some(url)) => format!("[#link(\"{}\")[{}]],", escape_typst_string(url), n),
        (Some(n), None) => format!("[#text(fill: luma(45%))[{}]],", n),
    }
}

/// The cell holding the diff line itself.
///
/// The text colour is set inside the link so it survives the template's
/// `show link` rule: a whole page of blue diff would be unreadable, while the
/// line numbers keep the link colour and show that the row can be clicked.
fn text_cell(line: &DiffLine, target: Option<&str>) -> String {
    let color = match line.kind {
        LineKind::Header | LineKind::Marker => "luma(45%)".to_string(),
        LineKind::Hunk => format!("rgb(\"{}\")", HUNK_COLOR),
        _ => "luma(15%)".to_string(),
    };
    let text = format!(
        "#text(fill: {}, \"{}\")",
        color,
        escape_typst_string(&line.text)
    );
    match target {
        Some(url) => format!("[#link(\"{}\")[{}]],", escape_typst_string(url), text),
        None => format!("[{}],", text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO_FILES: &str = "\
diff --git a/src/a.rs b/src/a.rs
index 1111111..2222222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -10,4 +10,5 @@ fn context()
 fn one() {}
-fn two() {}
+fn two(x: u8) {}
+fn three() {}
 fn four() {}
diff --git a/docs/new.md b/docs/new.md
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/docs/new.md
@@ -0,0 +1,2 @@
+# Title
+Body
";

    fn pr(diff: &str, files_url: Option<&str>) -> PRDiff {
        PRDiff {
            base_ref: "main".into(),
            head_ref: "feature".into(),
            diff: diff.into(),
            files_url: files_url.map(str::to_string),
        }
    }

    #[test]
    fn test_split_diff_separates_files() {
        let files = split_diff(TWO_FILES);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path(), "src/a.rs");
        assert_eq!(files[0].old_path.as_deref(), Some("src/a.rs"));
        assert_eq!(files[1].path(), "docs/new.md");
        // An added file has no old side.
        assert_eq!(files[1].old_path, None);
        assert!(!files[1].is_rename());
    }

    #[test]
    fn test_split_diff_counts_changed_lines() {
        let files = split_diff(TWO_FILES);
        assert_eq!((files[0].added(), files[0].removed()), (2, 1));
        assert_eq!((files[1].added(), files[1].removed()), (2, 0));
    }

    /// The line numbers are what the links are built from, so each side has
    /// to be counted the way GitHub counts it.
    #[test]
    fn test_split_diff_numbers_both_sides() {
        let lines = &split_diff(TWO_FILES)[0].lines;
        let numbered: Vec<_> = lines
            .iter()
            .filter(|l| l.kind != LineKind::Header && l.kind != LineKind::Hunk)
            .map(|l| (l.kind, l.old_line, l.new_line))
            .collect();
        assert_eq!(
            numbered,
            vec![
                (LineKind::Context, Some(10), Some(10)),
                (LineKind::Removed, Some(11), None),
                (LineKind::Added, None, Some(11)),
                (LineKind::Added, None, Some(12)),
                (LineKind::Context, Some(12), Some(13)),
            ]
        );
    }

    #[test]
    fn test_split_diff_keeps_content_without_a_git_header() {
        let files = split_diff("--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path(), "x");
    }

    #[test]
    fn test_split_diff_ignores_an_empty_diff() {
        assert!(split_diff("").is_empty());
        assert!(split_diff("\n\n").is_empty());
    }

    #[test]
    fn test_split_diff_reads_a_rename() {
        let files = split_diff(
            "diff --git a/old.rs b/new.rs\nsimilarity index 100%\nrename from old.rs\nrename to new.rs\n",
        );
        assert_eq!(files[0].old_path.as_deref(), Some("old.rs"));
        assert_eq!(files[0].new_path.as_deref(), Some("new.rs"));
        assert!(files[0].is_rename());
        // The anchor follows the file to its new path.
        assert_eq!(files[0].path(), "new.rs");
    }

    #[test]
    fn test_split_diff_reads_a_binary_file_from_the_git_header() {
        let files = split_diff(
            "diff --git a/img/logo.png b/img/logo.png\nindex 111..222 100644\nBinary files a/img/logo.png and b/img/logo.png differ\n",
        );
        assert_eq!(files[0].path(), "img/logo.png");
    }

    /// GitHub quotes paths with non-ASCII characters; the anchor has to be
    /// the SHA-256 of the decoded path, not of the quoted spelling.
    #[test]
    fn test_split_diff_unquotes_a_path() {
        let files = split_diff(
            "diff --git \"a/src/\\303\\251t\\303\\251.rs\" \"b/src/\\303\\251t\\303\\251.rs\"\n--- \"a/src/\\303\\251t\\303\\251.rs\"\n+++ \"b/src/\\303\\251t\\303\\251.rs\"\n@@ -1 +1 @@\n-a\n+b\n",
        );
        assert_eq!(files[0].path(), "src/été.rs");
    }

    /// The anchor GitHub gives a file in the diff view. Checked against the
    /// rendered page of a real pull request.
    #[test]
    fn test_file_anchor_is_the_sha256_of_the_path() {
        assert_eq!(
            file_anchor("Cargo.toml"),
            "diff-2e9d962a08321605940b5a657135052fbcef87b5e360662bb527c96d9a615542"
        );
        assert_eq!(
            file_anchor("src/pdf.rs"),
            "diff-5aa0fd799075a06f88dd37c6e087e1728118a487e44f3790f9b02e134ee08d62"
        );
    }

    #[test]
    fn test_pr_files_url() {
        assert_eq!(
            pr_files_url("iesahin", "gh2pdf", 9),
            "https://github.com/iesahin/gh2pdf/pull/9/files"
        );
    }

    /// Every file starts on a page of its own.
    #[test]
    fn test_render_pr_diff_breaks_a_page_per_file() {
        let md = render_pr_diff(&pr(TWO_FILES, None));
        let pages: Vec<&str> = md.split("PAGEBREAKPLACEHOLDER").collect();
        // The index, then one page per file.
        assert_eq!(pages.len(), 3);
        assert!(pages[0].contains("## PR Diff: main <- feature"));
        assert!(pages[1].contains("### `src/a.rs`"));
        assert!(!pages[1].contains("### `docs/new.md`"));
        assert!(pages[2].contains("### `docs/new.md`"));
    }

    #[test]
    fn test_render_pr_diff_indexes_the_files() {
        let md = render_pr_diff(&pr(TWO_FILES, Some("https://github.com/o/r/pull/3/files")));
        assert!(md.starts_with("## PR Diff: main <- feature\n\n"));
        assert!(md.contains(&format!(
            "- [`src/a.rs`](https://github.com/o/r/pull/3/files#{}) (+2, -1)\n",
            file_anchor("src/a.rs")
        )));
    }

    /// The point of the whole module: a line in the PDF opens the review view
    /// on that line, where its comment button is.
    #[test]
    fn test_render_pr_diff_links_lines_to_their_side() {
        let md = render_pr_diff(&pr(TWO_FILES, Some("https://github.com/o/r/pull/3/files")));
        let anchor = file_anchor("src/a.rs");
        // The added line is line 11 of the new file: right side.
        assert!(md.contains(&format!(
            "#link(\"https://github.com/o/r/pull/3/files#{}R11\")",
            anchor
        )));
        // The removed line is line 11 of the old file: left side.
        assert!(md.contains(&format!(
            "#link(\"https://github.com/o/r/pull/3/files#{}L11\")",
            anchor
        )));
    }

    /// Without a pull request URL the same document is produced, just
    /// unlinked — inboxbot renders issues that are not pull requests too.
    #[test]
    fn test_render_pr_diff_without_a_url_has_no_links() {
        let md = render_pr_diff(&pr(TWO_FILES, None));
        assert!(!md.contains("#link("));
        assert!(md.contains("PAGEBREAKPLACEHOLDER"));
        assert!(md.contains("fn three() {}"));
    }

    #[test]
    fn test_render_pr_diff_of_an_empty_diff_is_empty() {
        assert_eq!(render_pr_diff(&pr("", None)), "");
    }

    /// A one-line chunk still has to produce an array Typst can index, not a
    /// parenthesised value.
    #[test]
    fn test_render_pr_diff_writes_an_array_of_fills() {
        let md = render_pr_diff(&pr("diff --git a/x b/x\n", None));
        assert!(md.contains("#let fills = (n, )"));
    }

    /// Diff text reaches Typst inside a string literal, so quotes and
    /// backslashes in the code being reviewed must not end it.
    #[test]
    fn test_render_pr_diff_escapes_the_line_text() {
        let md = render_pr_diff(&pr(
            "--- a/x\n+++ b/x\n@@ -1 +1 @@\n+let s = \"a\\\\b\";\n",
            None,
        ));
        assert!(md.contains(r#""+let s = \"a\\\\b\";""#), "{}", md);
    }

    /// A CRLF diff must not carry the carriage return into the Typst source.
    #[test]
    fn test_split_diff_drops_carriage_returns() {
        let files = split_diff("--- a/x\r\n+++ b/x\r\n@@ -1 +1 @@\r\n+added\r\n");
        assert!(files[0].lines.iter().all(|l| !l.text.contains('\r')));
    }
}
