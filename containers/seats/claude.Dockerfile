# wicked seat image: claude. The engine builds it on first use (or `wicked-core seat-image build claude`)
# from this file alone (no build context), tagged by this file's hash. OSS base image; the CLI is its
# published npm package at the version the engine is verified against. At run time the user is the
# operator's uid (`--user`), the root filesystem is read-only and HOME is a tmpfs.
# WICKED_SEAT_BASE_IMAGE swaps the base for a project toolchain image (Debian-based).
ARG BASE=node:22-trixie-slim
FROM ${BASE}
RUN apt-get update \
 && apt-get install -y --no-install-recommends git ca-certificates curl procps ripgrep jq python3 make g++ \
 && (command -v npm >/dev/null || apt-get install -y --no-install-recommends nodejs npm) \
 && rm -rf /var/lib/apt/lists/*
RUN npm install -g --no-fund --no-audit @anthropic-ai/claude-code@2.1.287 && npm cache clean --force
LABEL wicked.seat.cli=claude wicked.seat.package=@anthropic-ai/claude-code@2.1.287
ENTRYPOINT []
