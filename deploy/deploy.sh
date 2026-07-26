#!/usr/bin/env bash
#
# Deploys the gh2pdf webhook server on a Debian VPS behind nginx.
#
# Installs the toolchain (Rust, pandoc, typst, fonts), builds gh2pdf from
# source, and sets up a systemd service proxied by nginx, optionally with a
# Let's Encrypt certificate. Safe to re-run: it updates the checkout,
# rebuilds, and reloads, but never overwrites /etc/gh2pdf/gh2pdf.env.
#
# Usage (as root):
#   ./deploy.sh --domain gh2pdf.example.com [--email you@example.com] \
#               [--port 8080] [--branch main] [--repo-url URL]
#
# After the first run:
#   1. Fill in /etc/gh2pdf/gh2pdf.env (App ID, webhook secret)
#   2. Upload the App's private key to /etc/gh2pdf/private-key.pem
#   3. systemctl start gh2pdf

set -euo pipefail

# --- Helpers ---------------------------------------------------------------

log() { echo -e "\n==> $*"; }

usage() {
    # Print the comment block at the top of this file (skipping the shebang).
    # The comment marker is stripped from a copy: editing $0 would make the
    # line stop matching /^#/ and end the block after its first line.
    awk 'NR > 1 && /^#/ { line = $0; sub(/^# ?/, "", line); print line }
         NR > 1 && !/^#/ { exit }' "$0"
    exit 1
}

# Succeeds when the nginx configuration on stdin has a server_name directive
# listing $1 as one of its names. This is what certbot's nginx installer
# looks for: without such a server block it refuses to install the
# certificate ("Could not automatically find a matching server block").
config_has_server_name() {
    awk -v domain="$1" '
        /^[[:space:]]*server_name[[:space:]]/ {
            names = $0
            sub(/#.*$/, "", names)
            sub(/;.*$/, "", names)
            n = split(names, field, /[[:space:]]+/)
            # Every field is compared: the empty leading field produced by
            # indentation and the directive name itself can never equal a
            # domain, so there is no need to know which index they land on.
            for (i = 1; i <= n; i++)
                if (field[i] == domain) { found = 1; exit }
        }
        END { exit found ? 0 : 1 }
    '
}

# deploy/tests sources this script to exercise the helpers above; everything
# below this point is the deployment itself and must not run then.
if [[ -n "${GH2PDF_DEPLOY_LIB_ONLY:-}" ]]; then
    return 0
fi

# --- Configuration ---------------------------------------------------------

REPO_URL="https://github.com/iesahin/gh2pdf"
BRANCH="main"
DOMAIN=""
EMAIL=""
PORT="8080"

PANDOC_VERSION="${PANDOC_VERSION:-3.6.3}"
TYPST_VERSION="${TYPST_VERSION:-0.13.1}"

SRC_DIR="/opt/gh2pdf/src"
BIN_PATH="/usr/local/bin/gh2pdf"
ETC_DIR="/etc/gh2pdf"
SERVICE_NAME="gh2pdf"
NGINX_SITE="/etc/nginx/sites-available/gh2pdf"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --domain) DOMAIN="$2"; shift 2 ;;
        --email) EMAIL="$2"; shift 2 ;;
        --port) PORT="$2"; shift 2 ;;
        --branch) BRANCH="$2"; shift 2 ;;
        --repo-url) REPO_URL="$2"; shift 2 ;;
        -h|--help) usage ;;
        *) echo "Unknown option: $1" >&2; usage ;;
    esac
done

if [[ -z "$DOMAIN" ]]; then
    echo "Error: --domain is required (nginx server_name and TLS certificate)" >&2
    usage
fi

if [[ $(id -u) -ne 0 ]]; then
    echo "Error: run this script as root" >&2
    exit 1
fi

case "$(dpkg --print-architecture)" in
    amd64) PANDOC_ARCH="amd64"; TYPST_ARCH="x86_64" ;;
    arm64) PANDOC_ARCH="arm64"; TYPST_ARCH="aarch64" ;;
    *) echo "Error: unsupported architecture $(dpkg --print-architecture)" >&2; exit 1 ;;
esac

# --- System packages -------------------------------------------------------

log "Installing system packages"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq \
    nginx certbot python3-certbot-nginx \
    curl git xz-utils ca-certificates \
    build-essential pkg-config \
    fonts-linuxlibertine

# --- pandoc (Debian's package can predate the Typst writer) ----------------

if pandoc --version 2>/dev/null | head -1 | grep -q "pandoc ${PANDOC_VERSION}"; then
    log "pandoc ${PANDOC_VERSION} already installed"
else
    log "Installing pandoc ${PANDOC_VERSION}"
    tmp_deb=$(mktemp --suffix=.deb)
    curl -fsSL -o "$tmp_deb" \
        "https://github.com/jgm/pandoc/releases/download/${PANDOC_VERSION}/pandoc-${PANDOC_VERSION}-1-${PANDOC_ARCH}.deb"
    dpkg -i "$tmp_deb"
    rm -f "$tmp_deb"
fi

# --- typst -----------------------------------------------------------------

if typst --version 2>/dev/null | grep -q "typst ${TYPST_VERSION}"; then
    log "typst ${TYPST_VERSION} already installed"
else
    log "Installing typst ${TYPST_VERSION}"
    tmp_dir=$(mktemp -d)
    curl -fsSL -o "$tmp_dir/typst.tar.xz" \
        "https://github.com/typst/typst/releases/download/v${TYPST_VERSION}/typst-${TYPST_ARCH}-unknown-linux-musl.tar.xz"
    tar -xJf "$tmp_dir/typst.tar.xz" -C "$tmp_dir"
    install -m 755 "$tmp_dir/typst-${TYPST_ARCH}-unknown-linux-musl/typst" /usr/local/bin/typst
    rm -rf "$tmp_dir"
fi

# --- Rust toolchain --------------------------------------------------------

if ! command -v cargo >/dev/null 2>&1; then
    log "Installing Rust toolchain"
    curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --no-modify-path
fi
export PATH="$HOME/.cargo/bin:$PATH"

# --- Build gh2pdf from source ----------------------------------------------

log "Fetching gh2pdf source (${BRANCH})"
if [[ -d "$SRC_DIR/.git" ]]; then
    git -C "$SRC_DIR" fetch origin "$BRANCH"
    git -C "$SRC_DIR" checkout -B "$BRANCH" "origin/$BRANCH"
else
    mkdir -p "$(dirname "$SRC_DIR")"
    git clone --branch "$BRANCH" "$REPO_URL" "$SRC_DIR"
fi

log "Building gh2pdf (this can take a while on a small VPS)"
cargo build --release --manifest-path "$SRC_DIR/Cargo.toml"

if systemctl is-active --quiet "$SERVICE_NAME"; then
    systemctl stop "$SERVICE_NAME"
    RESTART_SERVICE=1
else
    RESTART_SERVICE=0
fi
install -m 755 "$SRC_DIR/target/release/gh2pdf" "$BIN_PATH"

# --- Service user and configuration ----------------------------------------

if ! id gh2pdf >/dev/null 2>&1; then
    log "Creating gh2pdf system user"
    useradd --system --home-dir /var/lib/gh2pdf --shell /usr/sbin/nologin gh2pdf
fi

log "Installing configuration to ${ETC_DIR}"
mkdir -p "$ETC_DIR"
if [[ ! -f "$ETC_DIR/gh2pdf.env" ]]; then
    install -m 640 -g gh2pdf "$SRC_DIR/deploy/gh2pdf.env.example" "$ETC_DIR/gh2pdf.env"
    sed -i "s/^#GH2PDF_PORT=.*/GH2PDF_PORT=${PORT}/" "$ETC_DIR/gh2pdf.env"
    NEEDS_CONFIG=1
else
    NEEDS_CONFIG=0
fi
chgrp gh2pdf "$ETC_DIR"
chmod 750 "$ETC_DIR"

# --- systemd ---------------------------------------------------------------

log "Installing systemd service"
install -m 644 "$SRC_DIR/deploy/gh2pdf.service" "/etc/systemd/system/${SERVICE_NAME}.service"
systemctl daemon-reload
systemctl enable "$SERVICE_NAME"

# --- nginx -----------------------------------------------------------------

# An existing site file is kept only when it already serves $DOMAIN: certbot
# edits it in place when it installs the certificate, and re-running this
# script must not undo that. A file left from a run with a different --domain
# is regenerated instead — keeping it would leave nginx with no server block
# for $DOMAIN, which passes `nginx -t` but makes certbot's installer fail.
if [[ -f "$NGINX_SITE" ]] && config_has_server_name "$DOMAIN" < "$NGINX_SITE"; then
    log "nginx site already serves ${DOMAIN}, leaving it untouched"
else
    if [[ -f "$NGINX_SITE" ]]; then
        backup="${NGINX_SITE}.bak.$(date +%Y%m%d%H%M%S)"
        log "Existing nginx site does not serve ${DOMAIN}; backing it up to ${backup}"
        mv "$NGINX_SITE" "$backup"
    fi
    log "Configuring nginx for ${DOMAIN}"
    sed -e "s/DOMAIN_PLACEHOLDER/${DOMAIN}/g" -e "s/PORT_PLACEHOLDER/${PORT}/g" \
        "$SRC_DIR/deploy/nginx-gh2pdf.conf" > "$NGINX_SITE"
fi
ln -sf "$NGINX_SITE" /etc/nginx/sites-enabled/gh2pdf
nginx -t
systemctl reload nginx

# certbot only sees server blocks reachable from /etc/nginx/nginx.conf, so
# check the assembled configuration (not just our file) before asking for a
# certificate: failing here is much cheaper than failing after issuance.
if ! nginx -T 2>/dev/null | config_has_server_name "$DOMAIN"; then
    echo "Error: nginx does not load a server block with server_name ${DOMAIN}." >&2
    echo "  Check that /etc/nginx/nginx.conf includes /etc/nginx/sites-enabled/*" >&2
    echo "  and that /etc/nginx/sites-enabled/gh2pdf points at ${NGINX_SITE}." >&2
    exit 1
fi

if [[ -n "$EMAIL" ]]; then
    # A previous run may have obtained the certificate but failed to install
    # it; `certbot --nginx` would then report "not yet due for renewal" and
    # leave nginx on plain HTTP, so install the saved certificate directly.
    if [[ -f "/etc/letsencrypt/live/${DOMAIN}/fullchain.pem" ]]; then
        log "Installing existing Let's Encrypt certificate for ${DOMAIN}"
        certbot install --nginx --non-interactive --cert-name "$DOMAIN" --redirect
    else
        log "Requesting Let's Encrypt certificate for ${DOMAIN}"
        certbot --nginx --non-interactive --agree-tos -m "$EMAIL" -d "$DOMAIN" --redirect
    fi
else
    log "Skipping TLS setup (pass --email to enable certbot). GitHub webhooks require HTTPS."
fi

# --- Start -----------------------------------------------------------------

if [[ "$NEEDS_CONFIG" -eq 1 ]]; then
    cat <<EOF

Deployment finished, but the service is NOT started yet. To finish:

  1. Edit ${ETC_DIR}/gh2pdf.env: set GH2PDF_APP_ID and GH2PDF_WEBHOOK_SECRET
  2. Upload the GitHub App's private key:
       install -m 640 -g gh2pdf private-key.pem ${ETC_DIR}/private-key.pem
  3. Start the service:
       systemctl start ${SERVICE_NAME}
  4. Verify:
       curl -s https://${DOMAIN}/healthz

Point the GitHub App's webhook URL at: https://${DOMAIN}/webhook
EOF
elif [[ "$RESTART_SERVICE" -eq 1 ]]; then
    log "Restarting ${SERVICE_NAME}"
    systemctl start "$SERVICE_NAME"
    systemctl --no-pager --lines 5 status "$SERVICE_NAME" || true
else
    log "Starting ${SERVICE_NAME}"
    systemctl start "$SERVICE_NAME"
    systemctl --no-pager --lines 5 status "$SERVICE_NAME" || true
fi
