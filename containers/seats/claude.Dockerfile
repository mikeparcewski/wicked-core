# syntax=docker/dockerfile:1
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
# landrun v0.1.17 publishes raw ELF assets `landrun-linux-{amd64,arm64}` (no archive):
# verified at https://github.com/Zouuup/landrun/releases/expanded_assets/v0.1.17,
# including the per-asset SHA-256 digests below. TARGETARCH is supplied by BuildKit;
# dpkg also supports builders that do not supply it.
ARG TARGETARCH
RUN set -eu; arch="${TARGETARCH:-$(dpkg --print-architecture)}"; \
    case "$arch" in \
      amd64) sha=6ada66a06669e8994e174a7271af2db636308e55a0d6ec896cc7d326b46727f6 ;; \
      arm64) sha=365d8c8656c732b14d70e621d972cb733e490621fb5b9c0876376bcf457dbb5d ;; \
      *) echo "unsupported landrun architecture: $arch" >&2; exit 1 ;; \
    esac; \
    curl -fsSL "https://github.com/Zouuup/landrun/releases/download/v0.1.17/landrun-linux-$arch" -o /usr/local/bin/landrun; \
    echo "$sha  /usr/local/bin/landrun" | sha256sum -c -; \
    chmod 0755 /usr/local/bin/landrun

# The outer entry proves the canary tmpfs is writable without Landlock, then execs
# landrun. The inner entry and CLI share that SAME Landlock domain: a denied write
# cannot be a false pass caused by a read-only mount or an unprivileged uid.
RUN cat > /usr/local/bin/wicked-seat-entry <<'EOF'
#!/bin/sh
set -eu
fail() {
    printf '%s\n' "wicked-seat-entry: Landlock canary failed: $*; CLI refused" >&2
    exit 126
}
if [ "${1-}" = --check ]; then
    [ "$#" -ge 4 ] && [ "$2" = /wicked-canary/probe ] && [ "$3" = -- ] || fail 'invalid check argv'
    probe=$2
    shift 3
    [ "$(cat "$probe")" = wicked-seat-canary ] || fail 'preflight proof missing'
    scratch="/tmp/.wicked-seat-canary-$$"
    (umask 077; : > "$scratch") || fail '/tmp is not writable inside the policy'
    rm -f "$scratch" || fail '/tmp cleanup denied'
    if (printf '%s\n' escaped > "$probe") 2>/dev/null; then
        fail 'out-of-tree write succeeded; Landlock is not enforcing'
    fi
    printf '%s\n' 'wicked-seat-entry: Landlock canary passed' >&2
else
    [ "$#" -ge 3 ] && [ "$1" = /wicked-canary/probe ] && [ "$2" = -- ] && [ "$3" = landrun ] || fail 'invalid launch argv'
    # ABI 4 is required for TCP rules. --best-effort may otherwise retain only
    # filesystem rules. Linux syscall 444 is landlock_create_ruleset on both
    # supported architectures (x86 syscall_64.tbl / asm-generic unistd.h).
    # Python is already in the image; isolated mode ignores seat Python config.
    python3 -I -B -c 'import ctypes, os, sys; sys.exit(0 if os.uname().machine in ("x86_64", "aarch64") and ctypes.CDLL(None).syscall(444, None, 0, 1) >= 4 else 1)' \
        || fail 'Landlock ABI 4 or newer is required for TCP enforcement'
    printf '%s\n' wicked-seat-canary > "$1" || fail 'canary tmpfs is not writable before Landlock'
    shift 2
fi
exec "$@"
EOF
RUN chmod 0755 /usr/local/bin/wicked-seat-entry

RUN npm install -g --no-fund --no-audit @anthropic-ai/claude-code@2.1.287 && npm cache clean --force
LABEL wicked.seat.cli=claude wicked.seat.package=@anthropic-ai/claude-code@2.1.287
ENTRYPOINT []
