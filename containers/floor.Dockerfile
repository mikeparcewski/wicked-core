# syntax=docker/dockerfile:1
# wicked floor image: the engine's TRUSTED seat launcher. Never run as a seat: the engine mounts it
# read-only into every seat at /floor (`--mount type=image`) and makes /floor/launch the entrypoint,
# so no code from the seat image runs before the floor holds (DES-CONTAINER-FLOOR §3.1, §8). It is
# also the rootfs of the engine's image inspection (the seat image mounted as data, never run).
# Static binaries only, so it works in any seat image (glibc, musl, distroless):
#   /launch   the launcher (busybox sh): canaries, then landrun, then the CLI
#   /busybox  busybox 1.37 (musl, static)
#   /landrun  landrun v0.1.17 built static from source (the release asset links glibc)
# Built by the engine from this file alone (no build context), tagged by this file's hash.
FROM golang:1.25-alpine AS landrun
RUN CGO_ENABLED=0 GOFLAGS=-trimpath go install -ldflags='-s -w' github.com/zouuup/landrun/cmd/landrun@v0.1.17

FROM busybox:1.37-musl AS launch
RUN cat > /launch <<'EOF'
#!/floor/busybox sh
# The seat launcher. Outer: /floor/launch <canary> -- /floor/landrun ... -- /floor/launch --check <canary> -- <cli...>
# Inner (inside the Landlock domain): /floor/launch --check <canary> -- <cli...>
set -eu
bb=/floor/busybox
port=47913
fail() {
    $bb printf '%s\n' "wicked-seat-launch: floor canary failed: $*; CLI refused" >&2
    exit 126
}
if [ "${1-}" = --check ]; then
    [ "$#" -ge 4 ] && [ "$2" = /wicked-canary/probe ] && [ "$3" = -- ] || fail 'invalid check argv'
    probe=$2
    shift 3
    [ "$($bb cat "$probe")" = wicked-seat-canary ] || fail 'preflight proof missing'
    scratch="/tmp/.wicked-seat-canary-$$"
    (umask 077; : > "$scratch") || fail '/tmp is not writable inside the policy'
    $bb rm -f "$scratch" || fail '/tmp cleanup denied'
    if (printf '%s\n' escaped > "$probe") 2>/dev/null; then
        fail 'out-of-tree write succeeded; Landlock is not enforcing files'
    fi
    # A connect outside the grants must be refused BY LANDLOCK: EACCES at connect time, whether or
    # not anything listens there. Without TCP enforcement (Landlock ABI < 4 under --best-effort)
    # the same connect is refused by the empty port or reaches a listener, so anything but EACCES
    # refuses the CLI. busybox wget connects without a prior bind (its nc binds first, and Landlock
    # denies that bind too, which would hide whether connect is enforced).
    if out=$($bb wget -q -T 2 -O /dev/null "http://127.0.0.1:$port/" 2>&1); then
        fail 'an ungranted TCP connect succeeded; Landlock is not enforcing the network'
    fi
    case "$out" in
        *"can't connect to remote host"*": Permission denied") ;;
        *) fail "an ungranted TCP connect was not denied by Landlock: $out" ;;
    esac
    $bb printf '%s\n' 'wicked-seat-launch: floor canary passed' >&2
else
    [ "$#" -ge 3 ] && [ "$1" = /wicked-canary/probe ] && [ "$2" = -- ] && [ "$3" = /floor/landrun ] || fail 'invalid launch argv'
    printf '%s\n' wicked-seat-canary > "$1" || fail 'canary tmpfs is not writable before Landlock'
    shift 2
fi
exec "$@"
EOF
RUN chmod 0755 /launch

FROM scratch
COPY --from=launch /bin/busybox /busybox
COPY --from=landrun /go/bin/landrun /landrun
COPY --from=launch /launch /launch
LABEL wicked.floor=1
