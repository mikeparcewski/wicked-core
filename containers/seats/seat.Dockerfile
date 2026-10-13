# syntax=docker/dockerfile:1
# wicked seat image: one CLI on an OSS base. The engine builds it on first use (or `wicked-core
# seat-image build <cli>`) from this file alone (no build context), with CLI/CLI_PACKAGE naming the
# CLI's published npm package at the version the engine is verified against; tagged by the hash of
# this file, the package and the base. Nothing here runs before the floor holds: the engine's
# trusted launcher (containers/floor.Dockerfile, mounted read-only at /floor) is the entrypoint.
# At run time the user is the operator's uid (`--user`), the root filesystem is read-only and HOME
# is a tmpfs. WICKED_SEAT_BASE_IMAGE swaps the base for a project toolchain image (Debian-based).
ARG BASE=node:22-trixie-slim
FROM ${BASE}
ARG CLI
ARG CLI_PACKAGE
RUN apt-get update \
 && apt-get install -y --no-install-recommends git ca-certificates curl procps ripgrep jq python3 make g++ \
 && (command -v npm >/dev/null || apt-get install -y --no-install-recommends nodejs npm) \
 && rm -rf /var/lib/apt/lists/*
RUN test -n "$CLI_PACKAGE" && npm install -g --no-fund --no-audit "$CLI_PACKAGE" && npm cache clean --force
LABEL wicked.seat.cli=${CLI} wicked.seat.package=${CLI_PACKAGE}
ENTRYPOINT []
