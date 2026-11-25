# wicked seat toolchain layers (core#1043): go,java,python
# go (core#1043): Debian's golang-go. GOTOOLCHAIN=auto in its go.env, so a go.mod naming a
# newer go fetches that toolchain through the module proxy (Debian ships it set to local).
RUN set -eu; \
    apt-get update; \
    apt-get install -y --no-install-recommends golang-go; \
    rm -rf /var/lib/apt/lists/*; \
    env="$(go env GOROOT)/go.env"; \
    if grep -q '^GOTOOLCHAIN=' "$env"; then sed -i 's/^GOTOOLCHAIN=.*/GOTOOLCHAIN=auto/' "$env"; \
    else echo 'GOTOOLCHAIN=auto' >> "$env"; fi; \
    env -i PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin HOME=/nonexistent go version
# java (core#1043): Debian's OpenJDK 21 and Maven (a Gradle repository runs its own wrapper).
# A slim base has no man dirs, which the JDK's alternatives need.
RUN set -eu; \
    mkdir -p /usr/share/man/man1; \
    apt-get update; \
    apt-get install -y --no-install-recommends openjdk-21-jdk-headless maven; \
    rm -rf /var/lib/apt/lists/*; \
    env -i PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin HOME=/nonexistent sh -c 'java -version && javac -version && mvn -v'
# python (core#1043): uv 0.13.9, its release tarball checked against the sha256 pinned per
# architecture before it is unpacked (Debian does not package uv); uv and uvx root-owned and
# read-only in /usr/local/bin. python3, venv and pytest come from the seat image itself.
RUN set -eu; \
    case "$(dpkg --print-architecture)" in \
      amd64) t=x86_64-unknown-linux-gnu; sum=a7fbaff2b9a271d85c1bdb540941090fd1ce011797b4abeb48e4dcd39c2471d5 ;; \
      arm64) t=aarch64-unknown-linux-gnu; sum=0797542018e541a9288855df555c8077eed06f7a47d206362030480e8ecc9f3d ;; \
      *) echo "wicked-seat: no uv is pinned for $(dpkg --print-architecture)" >&2; exit 1 ;; \
    esac; \
    d="$(mktemp -d)"; \
    curl -fsSL --proto '=https' --tlsv1.2 --retry 3 -o "$d/uv.tar.gz" \
      "https://github.com/astral-sh/uv/releases/download/0.13.9/uv-$t.tar.gz"; \
    echo "$sum  $d/uv.tar.gz" | sha256sum -c -; \
    tar -xzf "$d/uv.tar.gz" -C "$d" --no-same-owner "uv-$t/uv" "uv-$t/uvx"; \
    install -o root -g root -m 0555 "$d/uv-$t/uv" "$d/uv-$t/uvx" /usr/local/bin/; \
    rm -rf "$d"; \
    env -i PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin HOME=/nonexistent uv --version
LABEL wicked.seat.layers="go,java,python"
