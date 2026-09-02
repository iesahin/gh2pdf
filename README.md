# gh2pdf

A GitHub App that converts issues and pull requests to PDFs and publishes
them as assets of a dedicated GitHub release. Whenever an issue or PR
changes — new comments, review comments, pushed commits, edits — the PDF is
re-rendered and a link to it is kept up to date in the issue/PR description.

The rendering pipeline is the same as
[inboxbot](https://github.com/iesahin/inboxbot)'s: the issue body, grouped
comments, review comments, and (for PRs) the full diff are assembled into
Markdown, converted to [Typst](https://typst.app) with
[Pandoc](https://pandoc.org), post-processed (page breaks between comment
groups, mermaid diagrams, remote image download), and compiled to PDF.

A pull request's diff closes the document with an index of the changed
files and then **one page per file**. Every file and every line of it is a
link into the PR's *Files changed* view, anchored at that exact line — so
reading the PDF away from the screen and tapping a line takes you straight
to where its review comment is written.

## How it works

1. A webhook event arrives for an issue or PR (`issues`, `issue_comment`,
   `pull_request`, `pull_request_review`, `pull_request_review_comment`).
2. gh2pdf authenticates as the App installation, fetches the full issue/PR
   context (body, comments, reviews, diff), and renders the PDF.
3. The PDF is uploaded as `<repo>-<number>-<title-slug>.pdf` to the release
   tagged `gh2pdf` (configurable). Older PDFs of the same issue/PR are
   replaced, so the release holds exactly one current PDF per issue.
4. A link block is inserted into (or updated in) the issue/PR description:

   ```
   <!-- gh2pdf:begin -->
   📄 [PDF](https://github.com/o/r/releases/download/gh2pdf/r-7-title.pdf) — updated 2026-07-19 12:00 UTC
   <!-- gh2pdf:end -->
   ```

   The HTML-comment markers make the update idempotent; the rest of the
   description is never touched.

## Setting up the GitHub App

1. Create a new GitHub App (Settings → Developer settings → GitHub Apps) with:
   - **Webhook URL**: `https://<your-host>/webhook`
   - **Webhook secret**: a random string, also passed to the server
   - **Repository permissions**:
     - Issues: Read & write (read comments, update descriptions)
     - Pull requests: Read & write
     - Contents: Read & write (create the release, upload assets, read
       `.github/gh2pdf.toml`)
   - **Subscribe to events**: Issues, Issue comment, Pull request,
     Pull request review, Pull request review comment
2. Generate a private key (PEM) and note the App ID.
3. Install the App on the repositories you want converted.
4. Run the server:

   ```bash
   gh2pdf serve \
     --app-id 123456 \
     --private-key /path/to/private-key.pem \
     --webhook-secret "$SECRET" \
     --port 8080
   ```

Every option can also be set through an environment variable (or a `.env`
file); see `gh2pdf serve --help`.

## PDF options

The parameters used to produce PDFs are set on the command line (or via
`GH2PDF_*` environment variables) and can be overridden per repository:

| Option | Env | Default | Meaning |
|---|---|---|---|
| `--release-tag` | `GH2PDF_RELEASE_TAG` | `gh2pdf` | Tag of the dedicated release storing the PDFs |
| `--omit-user` | `GH2PDF_OMIT_USER` | *(empty)* | Username omitted from comment headers |
| `--no-diff` | `GH2PDF_NO_DIFF` | off | Skip the PR diff section |
| `--timezone-offset` | `GH2PDF_TZ_OFFSET` | `3` | Hours from UTC for timestamps |
| `--template` | `GH2PDF_TEMPLATE` | built-in | Typst preamble template path |
| `--paper` | `GH2PDF_PAPER` | `a4` | Paper size of the built-in template |
| `--font` | `GH2PDF_FONT` | `Linux Libertine O` | Font of the built-in template |
| `--font-size` | `GH2PDF_FONT_SIZE` | `11` | Font size (pt) of the built-in template |
| `--no-description-link` | `GH2PDF_NO_DESCRIPTION_LINK` | off | Don't touch the description |

A custom `--template` file may use `TITLE_PLACEHOLDER`, `AUTHOR_PLACEHOLDER`
and `DATE_PLACEHOLDER` markers, which are substituted per document.

### Per-repository overrides

A repository can override any of these by committing `.github/gh2pdf.toml`.
Only the keys present in the file change; everything else keeps the server
defaults:

```toml
release_tag = "pdfs"
omit_user = "iesahin"
include_diff = false
timezone_offset_hours = 0
paper = "us-letter"
font = "Linux Libertine O"
font_size_pt = 10
link_description = true
```

## One-shot CLI conversion

The same pipeline runs without the App, using a personal access token —
equivalent to inboxbot's `gh2pdf` binary but publishing to a release instead
of rclone:

```bash
GITHUB_TOKEN=ghp_... gh2pdf convert https://github.com/owner/repo/issues/42
```

## Running with Docker

The multi-stage `Dockerfile` builds the release binary and produces a
Debian-slim image with pandoc, typst, and the Linux Libertine fonts baked in,
running as a non-root user:

```bash
docker build -t gh2pdf .
docker run -d -p 127.0.0.1:8080:8080 \
  --env-file gh2pdf.env \
  -e GH2PDF_PRIVATE_KEY=/run/secrets/github-app-key \
  -v ./private-key.pem:/run/secrets/github-app-key:ro \
  gh2pdf
```

Or use the included `docker-compose.yml`: copy
`deploy/gh2pdf.env.example` to `gh2pdf.env`, fill in the App ID and
webhook secret, drop the App's private key next to it as
`private-key.pem`, and run `docker compose up -d`. The container binds
to localhost only — put nginx, Caddy, or Traefik in front for TLS, since
GitHub webhooks require HTTPS. Pandoc/typst versions can be overridden at
build time with `--build-arg PANDOC_VERSION=... --build-arg
TYPST_VERSION=...`.

## Deploying on a Debian VPS

`deploy/deploy.sh` sets up everything on a fresh Debian server: pandoc,
typst, the Linux Libertine fonts, a Rust toolchain, a release build of gh2pdf, a
hardened systemd service running as a dedicated `gh2pdf` user, and an nginx
reverse proxy (with a Let's Encrypt certificate via certbot when `--email`
is given — GitHub requires HTTPS for webhooks):

```bash
sudo ./deploy.sh --domain gh2pdf.example.com --email you@example.com
```

The first run stops short of starting the service and tells you what to
fill in:

1. `/etc/gh2pdf/gh2pdf.env` — App ID and webhook secret
2. `/etc/gh2pdf/private-key.pem` — the App's private key
3. `systemctl start gh2pdf`, then check `https://<domain>/healthz`

The script is idempotent: re-running it pulls the latest branch, rebuilds,
and restarts the service, without touching your `gh2pdf.env` or the
certbot-managed nginx site. Changing `--domain` regenerates the nginx site
(the previous one is kept as `/etc/nginx/sites-available/gh2pdf.bak.<timestamp>`),
and a certificate that was issued but never installed — certbot's "Could not
install certificate" — is installed on the next run. Options: `--port`
(default 8080), `--branch` (default `main`), `--repo-url`. nginx exposes only
`/webhook` and `/healthz`; everything else returns 404.

## Building

```bash
cargo build --release
cargo test
```

Runtime dependencies (shelled out to, must be on `PATH`):

- `pandoc` — Markdown → Typst
- `typst` — Typst → PDF

## License

MIT
