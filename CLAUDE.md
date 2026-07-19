# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
# Build
cargo build
cargo build --release

# Test all
cargo test

# Test a single test by name
cargo test test_upsert_pdf_link

# Lint and format (fix all warnings before committing)
cargo clippy --all-targets
cargo fmt

# Run the webhook server
cargo run -- serve --app-id <id> --private-key key.pem --webhook-secret <secret>

# One-shot conversion with a PAT
GITHUB_TOKEN=... cargo run -- convert <github-issue-or-pr-url>
```

CI runs fmt, clippy (`-D warnings`), `cargo build --release`, and `cargo test` on push and PRs.

## Architecture

gh2pdf is a GitHub App that converts issues and PRs to PDFs and publishes
them as assets of a dedicated GitHub release, keeping a link to the PDF in
the issue/PR description. The rendering pipeline (Markdown assembly →
pandoc → Typst post-processing → typst compile) is ported from
[inboxbot](https://github.com/iesahin/inboxbot) so the output format is
identical.

### Module map (`src/`)

| Module | Responsibility |
|---|---|
| `main.rs` | CLI: `serve` (webhook server, App auth) and `convert` (one-shot with a PAT); all PDF options as clap args with `GH2PDF_*` env fallbacks |
| `config.rs` | `PdfOptions` (every PDF-production parameter) + `PdfOptionsPatch` (per-repo `.github/gh2pdf.toml` overrides) |
| `github.rs` | `GitHubProvider` trait hiding all GitHub access; `GitHubClient` (reqwest REST); `AppAuth` (App JWT → cached installation tokens); deep `publish_release_asset` (ensure release, purge stale assets, upload); `mock::MockGitHubClient` for tests |
| `models.rs` | `UnifiedComment`, `IssueContext`, `PRContext`, `PRDiff`; REST response and webhook payload types |
| `pdf.rs` | PDF pipeline: `assemble_markdown` → `compile_content_to_pdf` (pandoc → typst); `post_process_typst` (page breaks, mermaid, remote image download); `build_preamble` |
| `pipeline.rs` | `convert_and_publish`: fetch context → render → publish asset → upsert description link; `effective_options` merges repo config over server defaults |
| `description.rs` | Idempotent `upsert_pdf_link` using `<!-- gh2pdf:begin/end -->` markers |
| `webhook.rs` | axum server: HMAC signature verification, event relevance filter, own-bot loop prevention, per-issue serialisation locks |

### Key design decisions

- **`GitHubProvider` trait** — all GitHub access goes through
  `Arc<dyn GitHubProvider>`; tests use `github::mock::MockGitHubClient`.
- **Deep release publishing** — `publish_release_asset` is one call that
  finds/creates the release, deletes assets with the issue's
  `<repo>-<number>-` prefix (handles renamed titles), and uploads.
- **Loop prevention** — the app's own description edits arrive back as
  `edited` webhook events; events whose sender is the configured bot login
  (`--bot-login`, default `gh2pdf[bot]`) are skipped.
- **Per-issue locks** — concurrent webhook events about the same issue are
  serialised; each run fetches fresh state so the last run converges.
- **Scratch directories** — each conversion works in its own temp directory,
  so concurrent conversions never share intermediate files.

## Engineering standards

Follows Ousterhout's *A Philosophy of Software Design* (deep modules, simple
interfaces hiding complexity) and Beck's *Tidy First* (separate structural
from behavioural commits; every commit builds and passes tests).

- Use `anyhow` for error propagation with `?`; avoid `unwrap()`/`expect()` in library code
- Run `cargo fmt` and `cargo clippy --all-targets` and fix all warnings before committing
- Bump the patch version in `Cargo.toml` when completing a user-requested task
- Every bug fix must include a regression test

## External runtime dependencies

- `pandoc` — Markdown/HTML → Typst
- `typst` — Typst → PDF
