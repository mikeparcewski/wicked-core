# syntax=docker/dockerfile:1
# wicked checks floor image (DES-FLOOR-CONTAINER): the image the engine runs a repository's own
# checks in (`src/checks_container.rs`), one fresh container per floor side, with no network, a
# read-only root filesystem and the tree streamed in. Not a seat image and not the seat launcher
# (`containers/floor.Dockerfile`): nothing of a seat runs here, and no seat runs this.
# Built by the engine from this file alone (no build context), tagged by this file's hash, so an
# edit (a toolchain bump) is a new image.
#   node 22 + npm (the base), rust (pinned, with rustfmt and clippy), and the tools the suites spawn
#   that a slim base lacks: procps (`ps`), unzip, python3, git, a C toolchain for build scripts.
ARG BASE=node:22-trixie-slim
FROM ${BASE}
ARG RUST_TOOLCHAIN=1.99.1
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo
ENV PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl git procps unzip python3 \
      build-essential pkg-config libssl-dev \
 && rm -rf /var/lib/apt/lists/*
RUN curl -fsSL --proto '=https' --tlsv1.2 https://sh.rustup.rs \
      | sh -s -- -y --profile minimal --default-toolchain "$RUST_TOOLCHAIN" --component rustfmt,clippy \
 && chmod -R a+rX /usr/local/cargo /usr/local/rustup \
 && rustc --version && cargo --version && cargo clippy --version && cargo fmt --version
# The mount points the engine's cache volumes are first mounted on, owned by the checks' user
# (uid 1000, `node`): a fresh named volume takes this ownership on its first mount, so no root
# step is needed to make it writable by the install container.
RUN mkdir -p /wicked/vol /wicked/target /wicked/cargo-home \
 && chown 1000:1000 /wicked/vol /wicked/target /wicked/cargo-home
LABEL wicked.checks-floor.image=1
ENTRYPOINT []
CMD ["sleep", "infinity"]
