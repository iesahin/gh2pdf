# syntax=docker/dockerfile:1

# --- Build stage -----------------------------------------------------------

FROM rust:1-slim-bookworm AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src

# Cache the registry and build artifacts across image builds; copy the
# binary out because cache mounts are not part of the layer.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release && cp target/release/gh2pdf /usr/local/bin/gh2pdf

# --- Runtime stage ---------------------------------------------------------

FROM debian:bookworm-slim

# Debian's packaged pandoc predates the Typst writer, so pandoc and typst
# are installed from their upstream releases (same versions as
# deploy/deploy.sh).
ARG TARGETARCH
ARG PANDOC_VERSION=3.6.3
ARG TYPST_VERSION=0.13.1

RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
        ca-certificates curl xz-utils fonts-libertinus; \
    case "$TARGETARCH" in \
        amd64) TYPST_ARCH=x86_64 ;; \
        arm64) TYPST_ARCH=aarch64 ;; \
        *) echo "unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /tmp/pandoc.deb \
        "https://github.com/jgm/pandoc/releases/download/${PANDOC_VERSION}/pandoc-${PANDOC_VERSION}-1-${TARGETARCH}.deb"; \
    dpkg -i /tmp/pandoc.deb; \
    curl -fsSL -o /tmp/typst.tar.xz \
        "https://github.com/typst/typst/releases/download/v${TYPST_VERSION}/typst-${TYPST_ARCH}-unknown-linux-musl.tar.xz"; \
    tar -xJf /tmp/typst.tar.xz -C /tmp; \
    install -m 755 "/tmp/typst-${TYPST_ARCH}-unknown-linux-musl/typst" /usr/local/bin/typst; \
    rm -rf /tmp/pandoc.deb /tmp/typst.tar.xz "/tmp/typst-${TYPST_ARCH}-unknown-linux-musl"; \
    apt-get purge -y xz-utils; \
    apt-get autoremove -y; \
    rm -rf /var/lib/apt/lists/*; \
    pandoc --version | head -1; \
    typst --version

COPY --from=builder /usr/local/bin/gh2pdf /usr/local/bin/gh2pdf

# Typst downloads its packages (toffee-tufte, mmdr, ...) into the cache
# directory on first compile, so the user needs a writable home.
RUN useradd --system --create-home --home-dir /home/gh2pdf gh2pdf
USER gh2pdf
WORKDIR /home/gh2pdf
ENV XDG_CACHE_HOME=/home/gh2pdf/.cache

EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s \
    CMD curl -fsS "http://127.0.0.1:${GH2PDF_PORT:-8080}/healthz" || exit 1

ENTRYPOINT ["gh2pdf"]
CMD ["serve"]
