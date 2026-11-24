# syntax=docker/dockerfile:1
# wicked seat image: one CLI on the checks floor image (containers/checks/node22-rust.Dockerfile:
# node, the pinned Rust toolchain on the default PATH, git, a C toolchain), so every seat is
# Rust-capable. The engine composes the checks recipe as a named stage for the default build;
# an operator override is passed as BASE.
# The ARG default below is only for a build run by hand. The engine builds it on first use (or `wicked-core
# seat-image build <cli>`) without a build context, with CLI naming the CLI and
# either CLI_PACKAGE its published npm package, or (a CLI not published to npm: agy) CLI_TARBALL_*
# its vendor release tarball per architecture, each at the version the engine is verified against;
# tagged by the hash of this file, the package pin and the base. Nothing here runs before the floor holds: the engine's
# trusted launcher (containers/floor.Dockerfile, mounted read-only at /floor) is the entrypoint.
# At run time the user is the operator's uid (`--user`), the root filesystem is read-only and HOME
# is a tmpfs. WICKED_SEAT_BASE_IMAGE swaps the base for a project toolchain image (Debian-based).
ARG BASE=node:24.22.2-trixie-slim
FROM ${BASE}
# Python suites (core#1401): a seat runs `python3 -m pytest` as the floor does, from Debian's own
# archive (pytest, PyYAML, zstandard, venv): no PyPI, no new registry.
RUN apt-get update \
 && apt-get install -y --no-install-recommends git ca-certificates curl procps ripgrep jq python3 make g++ \
      python3-pytest python3-yaml python3-zstandard python3-venv \
 && (command -v npm >/dev/null || apt-get install -y --no-install-recommends nodejs npm) \
 && rm -rf /var/lib/apt/lists/*
# A headless browser and the media tools, from Debian's own archive (amd64 and arm64), in their own
# layer: a UI job takes its own screenshot evidence (core#1455: `chromium --screenshot`, or
# Playwright's `executablePath: '/usr/bin/chromium'`), and a demo review samples its own stills
# (core#1614: ffmpeg, ffprobe). The seat's floor has no display, no user namespaces and a read-only
# /dev (src/container_floor.rs landrun_prefix), so Debian's launcher reads the flags that make
# Chromium work there from /etc/chromium.d (headless, no Chromium sandbox, shared memory in /tmp).
# Best effort only where the base's archive has no candidate for them (core#1888): an operator
# base whose archive lacks them still builds, and its seats then have no browser. Any other failure
# (a mirror error, a partial download, an apt-cache that cannot answer) is retried once and then
# fails the build, because the image is tagged by this file, the pin and the base: a browserless
# image would be reused until one of them changes. An index fetch that failed is a failed update,
# never a missing candidate: apt 2.1.16 and later exit non-zero for it (APT::Update::Error-Mode=any;
# an older apt ignores the setting), and an older apt's warning for it fails the update too.
RUN set -e; \
    apt_update() { \
      rc=0; out=$(LC_ALL=C apt-get update -o APT::Update::Error-Mode=any 2>&1) || rc=$?; \
      printf '%s\n' "$out"; \
      [ "$rc" = 0 ] && ! printf '%s\n' "$out" | grep -Eq '^(W: (Failed to fetch|Some index files failed)|E: )'; \
    }; \
    browser_install() { apt-get install -y chromium fonts-liberation ffmpeg --no-install-recommends; }; \
    apt_update || { sleep 10; apt_update; } || exit 1; \
    missing=""; \
    for p in chromium fonts-liberation ffmpeg; do \
      policy=$(LC_ALL=C apt-cache policy "$p") || exit 1; \
      printf '%s\n' "$policy" | grep -q '^ *Candidate: [^(]' || missing="$missing $p"; \
    done; \
    if [ -n "$missing" ]; then \
      echo "wicked-seat: this base's archive has no$missing; its seats cannot take screenshots or sample video stills" >&2; \
    else \
      browser_install || { sleep 10; apt_update && browser_install; } || exit 1; \
      if test -d /etc/chromium.d; then \
        printf '%s\n' 'export CHROMIUM_FLAGS="$CHROMIUM_FLAGS --headless --no-sandbox --disable-dev-shm-usage --disable-gpu"' > /etc/chromium.d/wicked-seat; \
      else \
        echo "wicked-seat: this base's chromium reads no /etc/chromium.d; its seats' browser runs without the seat flags" >&2; \
      fi; \
    fi; \
    rm -rf /var/lib/apt/lists/*
# wicked-ledger at the version wicked-crew's QE acceptance check reads (core#1009). The QE unit
# records its verdict with it: the carrier hands that unit WICKED_LEDGER_PKG_DIR naming this copy
# (src/qe_acceptance.rs SEAT_LEDGER_PKG_DIR). A seat never installs it itself (it is fenced, and an
# install would change the tree under review), and the daemon's own copy is not mounted in, because
# its SQLite addon is built for the host, not for this Linux image. Best effort: a base image this
# cannot install on still builds, and its QE unit then reports the ledger as unavailable.
ARG LEDGER_PACKAGE=wicked-ledger@0.6.1
RUN mkdir -p /opt/wicked-ledger \
 && (npm install --prefix /opt/wicked-ledger --no-fund --no-audit --omit=dev "$LEDGER_PACKAGE" \
     || { rm -rf /opt/wicked-ledger; echo "wicked-seat: $LEDGER_PACKAGE did not install; QE verdicts cannot be recorded in this seat" >&2; }) \
 && npm cache clean --force
# The CLI: its npm package, or its vendor tarball. A tarball is fetched over https only, checked
# against the sha256 the engine pins for this architecture BEFORE anything of it is unpacked (the
# vendor signs nothing; the pin is the trust), and its one binary is installed root-owned and
# read-only at /usr/local/bin/<cli> (agy skips its self-update on a read-only install dir, and the
# root filesystem is read-only at run time as well).
# The CLI's arguments are declared here, after every shared layer: each RUN after an ARG carries
# it in its cache key, so a layer above them is built and stored once for all CLIs, not per CLI.
ARG CLI
ARG CLI_PACKAGE=
ARG CLI_PIN=
ARG CLI_TARBALL_MEMBER=
ARG CLI_TARBALL_URL_AMD64=
ARG CLI_TARBALL_SHA256_AMD64=
ARG CLI_TARBALL_URL_ARM64=
ARG CLI_TARBALL_SHA256_ARM64=
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
# The seat fence's push helper (core#1210). The fence re-aims every push at this helper
# (crates/wicked-apps-core/src/spawn.rs nopush_git_config_entries: the empty-prefix
# url.wicked-nopush::.pushInsteadOf, core#1785), so the remote's own URL arrives here byte for byte,
# in git's own working directory: a relative path means what it would have meant to git.
# One push is let through: to a bare repository STRICTLY inside the seat's own scratch dir (/tmp,
# the engine's tmpfs or emptyDir), resolved with every symlink and `..` followed, so a repository's
# test fixtures can push to a bare remote they made there. Everything else is refused, as before.
# This is the fence's delivery check, not a filesystem boundary: receive-pack runs as the seat,
# inside the seat's own floor, so it can write nothing the seat could not write itself.
RUN cat > /usr/local/bin/git-remote-wicked-nopush <<'EOF'
#!/bin/sh
# git-remote-wicked-nopush <remote> <url>: a git remote helper (`connect` only).
set -u
unset CDPATH GIT_DIR GIT_WORK_TREE GIT_COMMON_DIR GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_NAMESPACE GIT_CEILING_DIRECTORIES GIT_DISCOVERY_ACROSS_FILESYSTEM
scratch_root=/tmp
nl='
'
refuse() {
    printf '%s\n' "wicked-nopush: $1; a worker seat pushes only to a bare repository inside its own scratch dir ($scratch_root), delivery is the deliver phase's job" >&2
    exit 128
}
# The directory a path names, every component resolved (symlinks, `..`), kept byte for byte: a
# sentinel keeps command substitution from eating a trailing newline of the name.
resolve() {
    r=$(cd -P -- "$1" 2>/dev/null && pwd -P && printf x) || return 1
    r=${r%x}
    r=${r%"$nl"}
    case "$r" in *"$nl"*) return 1 ;; esac
    printf '%s' "$r"
}
url=${2-}
case "$url" in
    '') refuse "an empty push URL refused" ;;
    *"$nl"*) refuse "a push URL with a newline refused" ;;
    file:///*) path=${url#file://} ;;
    *://* | *::*) refuse "push to a remote ($url) refused" ;;
    *) path=$url ;;
esac
# git's own rule: a colon before the first slash is an ssh remote (host:path), never a path.
case "${path%%/*}" in *:*) refuse "push to a remote ($url) refused" ;; esac
root=$(resolve "$scratch_root") || refuse "the scratch dir is missing"
[ "$root" = "$scratch_root" ] && [ ! -L "$scratch_root" ] || refuse "the scratch dir $scratch_root is not a real directory"
real=$(resolve "$path") || refuse "push target $path does not resolve to a directory"
case "$real" in
    "$root"/?*) ;;
    *) refuse "push target $path resolves outside the scratch dir" ;;
esac
# receive-pack would enter <dir>/.git before <dir> itself.
[ ! -e "$real/.git" ] && [ ! -L "$real/.git" ] || refuse "push target $path holds a .git entry"
[ "$(git --git-dir="$real" rev-parse --is-bare-repository 2>/dev/null)" = true ] || refuse "push target $path is not a bare repository"
while IFS= read -r line; do
    case "$line" in
        capabilities) printf 'connect\n\n' ;;
        'connect git-receive-pack')
            printf '\n'
            exec git receive-pack "$real" ;;
        '') exit 0 ;;
        *) refuse "unsupported request: $line" ;;
    esac
done
EOF
RUN chmod 0755 /usr/local/bin/git-remote-wicked-nopush
LABEL wicked.seat.cli=${CLI} wicked.seat.package=${CLI_PIN}
ENTRYPOINT []
