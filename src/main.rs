use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use dotenv::dotenv;
use gh2pdf::config::PdfOptions;
use gh2pdf::github::{parse_issue_url, AppAuth, GitHubClient};
use gh2pdf::pipeline;
use gh2pdf::webhook::{self, AppState};
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "gh2pdf",
    version,
    about = "Converts GitHub issues and pull requests to PDFs and publishes them as release assets"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the GitHub App webhook server.
    Serve(ServeArgs),
    /// Convert a single issue or PR URL using a personal access token.
    Convert(ConvertArgs),
}

/// Options that describe the parameters used to produce PDFs. Each can also
/// be set through the corresponding GH2PDF_* environment variable, and
/// overridden per repository via `.github/gh2pdf.toml`.
#[derive(Args, Clone)]
struct PdfArgs {
    /// Tag of the dedicated release that stores the generated PDFs.
    #[arg(long, env = "GH2PDF_RELEASE_TAG", default_value = "gh2pdf")]
    release_tag: String,

    /// Username omitted from comment headers in the PDF.
    #[arg(long, env = "GH2PDF_OMIT_USER", default_value = "")]
    omit_user: String,

    /// Do not include the PR diff in the PDF.
    #[arg(long, env = "GH2PDF_NO_DIFF")]
    no_diff: bool,

    /// Timezone offset (hours from UTC) for timestamps in the PDF.
    #[arg(long, env = "GH2PDF_TZ_OFFSET", default_value_t = 3)]
    timezone_offset: i32,

    /// Path to a Typst preamble template (TITLE_PLACEHOLDER,
    /// AUTHOR_PLACEHOLDER and DATE_PLACEHOLDER are substituted).
    #[arg(long, env = "GH2PDF_TEMPLATE")]
    template: Option<String>,

    /// Paper size for the built-in template (e.g. a4, us-letter).
    #[arg(long, env = "GH2PDF_PAPER", default_value = "a4")]
    paper: String,

    /// Text font for the built-in template.
    #[arg(long, env = "GH2PDF_FONT", default_value = "Libertinus Serif")]
    font: String,

    /// Font size in points for the built-in template.
    #[arg(long, env = "GH2PDF_FONT_SIZE", default_value_t = 11)]
    font_size: u32,

    /// Do not add/update the PDF link in the issue/PR description.
    #[arg(long, env = "GH2PDF_NO_DESCRIPTION_LINK")]
    no_description_link: bool,
}

impl From<PdfArgs> for PdfOptions {
    fn from(args: PdfArgs) -> Self {
        Self {
            release_tag: args.release_tag,
            omit_user: args.omit_user,
            include_diff: !args.no_diff,
            timezone_offset_hours: args.timezone_offset,
            template_path: args.template,
            paper: args.paper,
            font: args.font,
            font_size_pt: args.font_size,
            link_description: !args.no_description_link,
        }
    }
}

#[derive(Args)]
struct ServeArgs {
    /// GitHub App ID.
    #[arg(long, env = "GH2PDF_APP_ID")]
    app_id: String,

    /// Path to the GitHub App's RSA private key (PEM).
    #[arg(long, env = "GH2PDF_PRIVATE_KEY")]
    private_key: String,

    /// Webhook secret configured on the GitHub App.
    #[arg(long, env = "GH2PDF_WEBHOOK_SECRET")]
    webhook_secret: String,

    /// Port to listen on.
    #[arg(long, env = "GH2PDF_PORT", default_value_t = 8080)]
    port: u16,

    /// Login of this app's bot account; its own events are ignored.
    #[arg(long, env = "GH2PDF_BOT_LOGIN", default_value = "gh2pdf[bot]")]
    bot_login: String,

    #[command(flatten)]
    pdf: PdfArgs,
}

#[derive(Args)]
struct ConvertArgs {
    /// GitHub issue or PR URL, e.g. https://github.com/owner/repo/issues/1
    url: String,

    /// GitHub token with repo scope.
    #[arg(long, env = "GITHUB_TOKEN")]
    token: String,

    #[command(flatten)]
    pdf: PdfArgs,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "info");
    }
    pretty_env_logger::init();

    match Cli::parse().command {
        Command::Serve(args) => serve(args).await,
        Command::Convert(args) => convert(args).await,
    }
}

async fn serve(args: ServeArgs) -> Result<()> {
    let pem = std::fs::read(&args.private_key)
        .with_context(|| format!("reading private key {}", args.private_key))?;
    let app_auth = Arc::new(AppAuth::new(args.app_id, &pem)?);
    let state = Arc::new(AppState::new(
        app_auth,
        args.webhook_secret,
        args.pdf.into(),
        args.bot_login,
    ));
    webhook::serve(state, args.port).await
}

async fn convert(args: ConvertArgs) -> Result<()> {
    let (owner, repo, number) = parse_issue_url(&args.url)?;
    let github = Arc::new(GitHubClient::with_token(args.token));
    let options: PdfOptions = args.pdf.into();

    println!("Converting {} to PDF...", args.url);
    let published = pipeline::convert_and_publish(github, &options, &owner, &repo, number).await?;
    println!("Published {} at {}", published.filename, published.url);
    Ok(())
}
