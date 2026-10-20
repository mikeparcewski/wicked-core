# syntax=docker/dockerfile:1
# wicked seat image: one CLI on an OSS base. The engine builds it on first use (or `wicked-core
# seat-image build <cli>`) from this file alone (no build context), with CLI naming the CLI and
# either CLI_PACKAGE its published npm package, or (a CLI not published to npm: agy) CLI_TARBALL_*
# its vendor release tarball per architecture, each at the version the engine is verified against;
# tagged by the hash of this file, the package pin and the base. Nothing here runs before the floor holds: the engine's
# trusted launcher (containers/floor.Dockerfile, mounted read-only at /floor) is the entrypoint.
# At run time the user is the operator's uid (`--user`), the root filesystem is read-only and HOME
# is a tmpfs. WICKED_SEAT_BASE_IMAGE swaps the base for a project toolchain image (Debian-based).
ARG BASE=node:22-trixie-slim
FROM ${BASE}
ARG CLI
ARG CLI_PACKAGE=
ARG CLI_PIN=
ARG CLI_TARBALL_MEMBER=
ARG CLI_TARBALL_URL_AMD64=
ARG CLI_TARBALL_SHA256_AMD64=
ARG CLI_TARBALL_URL_ARM64=
ARG CLI_TARBALL_SHA256_ARM64=
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
# The CLI: its npm package, or its vendor tarball. A tarball is fetched over https only, checked
# against the sha256 the engine pins for this architecture BEFORE anything of it is unpacked (the
# vendor signs nothing; the pin is the trust), and its one binary is installed root-owned and
# read-only at /usr/local/bin/<cli> (agy skips its self-update on a read-only install dir, and the
# root filesystem is read-only at run time as well).
RUN set -eu; \
    if [ -n "$CLI_PACKAGE" ]; then \
      npm install -g --no-fund --no-audit "$CLI_PACKAGE"; npm cache clean --force; \
    else \
      arch="$(dpkg --print-architecture)"; \
      case "$arch" in \
        amd64) url="$CLI_TARBALL_URL_AMD64"; sum="$CLI_TARBALL_SHA256_AMD64" ;; \
        arm64) url="$CLI_TARBALL_URL_ARM64"; sum="$CLI_TARBALL_SHA256_ARM64" ;; \
        *) echo "wicked-seat: no $CLI tarball is pinned for $arch" >&2; exit 1 ;; \
      esac; \
      test -n "$CLI" && test -n "$url" && test -n "$sum" && test -n "$CLI_TARBALL_MEMBER"; \
      d="$(mktemp -d)"; \
      curl -fsSL --proto '=https' --tlsv1.2 --retry 3 -o "$d/cli.tar.gz" "$url"; \
      echo "$sum  $d/cli.tar.gz" | sha256sum -c -; \
      tar -xzf "$d/cli.tar.gz" -C "$d" --no-same-owner "$CLI_TARBALL_MEMBER"; \
      install -o root -g root -m 0555 "$d/$CLI_TARBALL_MEMBER" "/usr/local/bin/$CLI"; \
      rm -rf "$d"; \
    fi
LABEL wicked.seat.cli=${CLI} wicked.seat.package=${CLI_PIN}
ENTRYPOINT []
