use serde::{Deserialize, Serialize};

/// Every parameter that controls how a PDF is produced.
///
/// Values are resolved in three layers, each overriding the previous:
/// 1. the defaults below,
/// 2. CLI options / environment variables (see `gh2pdf --help`),
/// 3. a `.github/gh2pdf.toml` committed to the repository being converted
///    (parsed as a [`PdfOptionsPatch`], so only the keys present override).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PdfOptions {
    /// Tag of the dedicated release that stores the generated PDFs.
    pub release_tag: String,
    /// Username omitted from comment headers in the PDF (usually the
    /// repository owner, so their own comments read as a diary).
    pub omit_user: String,
    /// Include the full PR diff as the last section of the PDF.
    pub include_diff: bool,
    /// Timezone offset (hours from UTC) used for timestamps in the PDF.
    pub timezone_offset_hours: i32,
    /// Path to a Typst preamble template with TITLE_PLACEHOLDER,
    /// AUTHOR_PLACEHOLDER and DATE_PLACEHOLDER markers. When unset, a
    /// built-in template is used.
    pub template_path: Option<String>,
    /// Paper size for the built-in template (e.g. "a4", "us-letter").
    pub paper: String,
    /// Text font for the built-in template.
    pub font: String,
    /// Font size in points for the built-in template.
    pub font_size_pt: u32,
    /// Add/update a link to the PDF in the issue/PR description.
    pub link_description: bool,
}

impl Default for PdfOptions {
    fn default() -> Self {
        Self {
            release_tag: "gh2pdf".to_string(),
            omit_user: String::new(),
            include_diff: true,
            timezone_offset_hours: 3,
            template_path: None,
            paper: "a4".to_string(),
            font: "Libertinus Serif".to_string(),
            font_size_pt: 11,
            link_description: true,
        }
    }
}

/// Partial override of [`PdfOptions`]: only the keys present in a repo's
/// `.github/gh2pdf.toml` change the server-wide options.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PdfOptionsPatch {
    pub release_tag: Option<String>,
    pub omit_user: Option<String>,
    pub include_diff: Option<bool>,
    pub timezone_offset_hours: Option<i32>,
    pub template_path: Option<String>,
    pub paper: Option<String>,
    pub font: Option<String>,
    pub font_size_pt: Option<u32>,
    pub link_description: Option<bool>,
}

impl PdfOptions {
    /// Returns a copy of `self` with every key present in `patch` overridden.
    pub fn with_patch(&self, patch: PdfOptionsPatch) -> Self {
        Self {
            release_tag: patch
                .release_tag
                .unwrap_or_else(|| self.release_tag.clone()),
            omit_user: patch.omit_user.unwrap_or_else(|| self.omit_user.clone()),
            include_diff: patch.include_diff.unwrap_or(self.include_diff),
            timezone_offset_hours: patch
                .timezone_offset_hours
                .unwrap_or(self.timezone_offset_hours),
            template_path: patch.template_path.or_else(|| self.template_path.clone()),
            paper: patch.paper.unwrap_or_else(|| self.paper.clone()),
            font: patch.font.unwrap_or_else(|| self.font.clone()),
            font_size_pt: patch.font_size_pt.unwrap_or(self.font_size_pt),
            link_description: patch.link_description.unwrap_or(self.link_description),
        }
    }
}

/// Repository path where per-repo option overrides live.
pub const REPO_CONFIG_PATH: &str = ".github/gh2pdf.toml";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_patch_overrides_only_present_keys() {
        let base = PdfOptions::default();
        let patch: PdfOptionsPatch = toml::from_str(
            r#"
            release_tag = "pdfs"
            include_diff = false
            "#,
        )
        .unwrap();
        let merged = base.with_patch(patch);
        assert_eq!(merged.release_tag, "pdfs");
        assert!(!merged.include_diff);
        // Untouched keys keep their defaults.
        assert_eq!(merged.paper, "a4");
        assert_eq!(merged.timezone_offset_hours, 3);
        assert!(merged.link_description);
    }

    #[test]
    fn test_patch_rejects_unknown_keys() {
        let result: Result<PdfOptionsPatch, _> = toml::from_str("no_such_option = true");
        assert!(result.is_err());
    }
}
