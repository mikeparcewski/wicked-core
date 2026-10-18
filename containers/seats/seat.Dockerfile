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
# wicked-ledger at the version wicked-crew's QE acceptance check reads (core#1009). The QE unit
# records its verdict with it: the carrier hands that unit WICKED_LEDGER_PKG_DIR naming this copy
# (src/qe_acceptance.rs SEAT_LEDGER_PKG_DIR). A seat never installs it itself (it is fenced, and an
# install would change the tree under review), and the daemon's own copy is not mounted in, because
# its SQLite addon is built for the host, not for this Linux image. Best effort: a base image this
# cannot install on still builds, and its QE unit then reports the ledger as unavailable.
ARG LEDGER_PACKAGE=wicked-ledger@0.6.0
RUN mkdir -p /opt/wicked-ledger \
 && (npm install --prefix /opt/wicked-ledger --no-fund --no-audit --omit=dev "$LEDGER_PACKAGE" \
     || { rm -rf /opt/wicked-ledger; echo "wicked-seat: $LEDGER_PACKAGE did not install; QE verdicts cannot be recorded in this seat" >&2; }) \
 && npm cache clean --force
RUN test -n "$CLI_PACKAGE" && npm install -g --no-fund --no-audit "$CLI_PACKAGE" && npm cache clean --force
LABEL wicked.seat.cli=${CLI} wicked.seat.package=${CLI_PACKAGE}
ENTRYPOINT []
