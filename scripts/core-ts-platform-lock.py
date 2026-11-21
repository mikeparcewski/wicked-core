#!/usr/bin/env python3
"""Re-pin the wicked-core-ts platform entries in crates/wicked-core-ts/package-lock.json (core#1551).

Usage:
  core-ts-platform-lock.py drop <package-lock.json>
  core-ts-platform-lock.py check <package-lock.json> <version>

The platform packages are the root's `optionalDependencies` named `wicked-core-ts-<platform>`.

The release PR writes each platform entry with `"version": "<ver>"` and no `resolved`/`integrity`: the
packages are not on npm until the tag publishes them, and `npm ci` accepts that shape both before and
after publication (core#1550). `npm install --package-lock-only` treats such an entry as satisfied and
leaves it alone, so napi-release.yml's lockfile job first DROPS the entries, regenerates, and then
CHECKS that every entry carries the released version, its registry tarball URL and an integrity hash.

  drop   deletes every `node_modules/wicked-core-ts-<platform>` entry (the root's pins stay), so the
         regeneration resolves each one afresh against the registry.
  check  each platform entry exists with `version` == <version>, `resolved` ==
         https://registry.npmjs.org/<name>/-/<name>-<version>.tgz and a `sha512-` integrity.

Exit codes: 0 done / all pinned; 1 check found a missing or wrong pin (each one is named on stderr);
2 bad usage or an unreadable lockfile. Stdlib only.
"""

import json
import sys

PREFIX = "wicked-core-ts-"
REGISTRY = "https://registry.npmjs.org"


def platforms(lock):
    """The platform package names the root pins, sorted."""
    root = lock.get("packages", {}).get("", {})
    names = sorted(n for n in root.get("optionalDependencies", {}) if n.startswith(PREFIX))
    if not names:
        raise ValueError("the lockfile's root pins no wicked-core-ts-<platform> package")
    return names


def drop(lock):
    """Delete each platform entry; returns the names dropped."""
    packages = lock["packages"]
    dropped = []
    for name in platforms(lock):
        if packages.pop(f"node_modules/{name}", None) is not None:
            dropped.append(name)
    return dropped


def check(lock, version):
    """Every problem with the platform pins, as one line each; empty when all are pinned."""
    packages = lock.get("packages", {})
    problems = []
    for name in platforms(lock):
        entry = packages.get(f"node_modules/{name}")
        if entry is None:
            problems.append(f"{name}: no lock entry")
            continue
        if entry.get("version") != version:
            problems.append(f"{name}: version {entry.get('version')!r}, expected {version!r}")
        tarball = f"{REGISTRY}/{name}/-/{name}-{version}.tgz"
        if entry.get("resolved") != tarball:
            problems.append(f"{name}: resolved {entry.get('resolved')!r}, expected {tarball!r}")
        integrity = entry.get("integrity")
        if not isinstance(integrity, str) or not integrity.startswith("sha512-"):
            problems.append(f"{name}: integrity {integrity!r}, expected a sha512- hash")
    return problems


def _load(path):
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def main(argv):
    try:
        if len(argv) == 2 and argv[0] == "drop":
            lock = _load(argv[1])
            dropped = drop(lock)
            with open(argv[1], "w", encoding="utf-8") as f:
                # npm's own shape: two-space indent, a trailing newline.
                f.write(json.dumps(lock, indent=2, ensure_ascii=False) + "\n")
            print(f"dropped {len(dropped)} platform entries: {', '.join(dropped) or 'none'}")
            return 0
        if len(argv) == 3 and argv[0] == "check":
            problems = check(_load(argv[1]), argv[2])
            for line in problems:
                print(f"::error::{line}", file=sys.stderr)
            if problems:
                return 1
            print(f"every platform entry is pinned to {argv[2]} (resolved + integrity)")
            return 0
    except (OSError, ValueError, KeyError) as e:
        print(f"core-ts-platform-lock: {e}", file=sys.stderr)
        return 2
    print(__doc__.split("\n\n")[1], file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
