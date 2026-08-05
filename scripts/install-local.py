#!/usr/bin/env python3
"""Build wicked-core and install it where the engine will actually find it — then PROVE it landed.

Why this exists
---------------

FINDING-081, five recorded instances: *the artifact that RUNS is not the artifact that was BUILT.*
Until now there was no install step at all. The binary reached ``~/.local/bin`` by hand, which is
precisely how that family keeps recurring — an undocumented manual copy has no version, no record,
and no way to fail.

Measured on a live host while this script was being written:

    installed binary seeds:  e7f84b91d030fdcc     <- pre-#190
    source constant expects: adaf3e9b6d088f1a

The installed CLI was five merges behind. It still answered ``--help``, still reported gate protocol
1, and still looked healthy — while every ``domain-extraction`` run on that host would have failed
closed on an unresolvable pin, because crew writes a drop-in carrying the NEW pin and the installed
CLI's vault only holds the OLD one. That is FINDING-080's failure mode, reached by doing nothing
wrong except forgetting to reinstall.

So copying is not the job. **Verifying is the job.** This script fails loudly when the binary it
just installed does not agree with the source it was built from, which turns that whole family from
a thing you have to remember into a checked property.

Cross-platform per CLAUDE.md: Python rather than shell, ``pathlib`` rather than path strings, and
the Windows ``.exe`` suffix and install directory handled explicitly rather than assumed away.

Usage
-----

    python3 scripts/install-local.py              # build (release), install, verify
    python3 scripts/install-local.py --check      # verify ONLY; installs nothing, exits 1 on skew
    python3 scripts/install-local.py --dest DIR   # override the install directory
"""

from __future__ import annotations

import argparse
import os
import platform
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
IS_WINDOWS = platform.system() == "Windows"
EXE = "wicked-core.exe" if IS_WINDOWS else "wicked-core"


def default_dest() -> Path:
    """Where a user-local binary belongs on this platform.

    ``~/.local/bin`` is the XDG convention and is already on PATH for most Unix users. Windows has
    no equivalent, so use the per-user location that installers there actually use; ``%LOCALAPPDATA%``
    is set on every supported Windows and falls back to the profile directory if it somehow is not.
    """
    if IS_WINDOWS:
        base = os.environ.get("LOCALAPPDATA") or str(Path.home())
        return Path(base) / "Programs" / "wicked"
    return Path.home() / ".local" / "bin"


def source_pin() -> str:
    """The coverage validator pin the SOURCE declares.

    Read out of the source rather than restated here: a second copy of a constant that must agree is
    the defect this script exists to catch, and writing one into the checker would be absurd.
    """
    src = (REPO / "src" / "domain_extraction.rs").read_text(encoding="utf-8")
    m = re.search(r'COVERAGE_VALIDATOR_PIN:\s*&str\s*=\s*"([0-9a-f]+)"', src)
    if not m:
        sys.exit(
            "could not find COVERAGE_VALIDATOR_PIN in src/domain_extraction.rs — the constant "
            "moved or was renamed; update this check rather than deleting it."
        )
    return m.group(1)


def installed_pin(binary: Path) -> str:
    """What the BINARY seeds, asked of the binary itself.

    Seeded into a throwaway database: this must never touch operator state to answer a question
    about a build.
    """
    with tempfile.TemporaryDirectory() as tmp:
        db = Path(tmp) / "verify.db"
        try:
            out = subprocess.run(
                [str(binary), "seed-domain-validators", "--db", str(db)],
                capture_output=True,
                text=True,
                timeout=120,
            )
        except (OSError, subprocess.SubprocessError) as e:
            sys.exit(f"could not run `{binary} seed-domain-validators`: {e}")
        blob = f"{out.stdout}\n{out.stderr}"
        m = re.search(r"pin:\s*([0-9a-f]{16})", blob)
        if not m:
            sys.exit(
                f"`{binary} seed-domain-validators` printed no pin. Output:\n"
                f"{blob[:400]}\n"
                "A binary that cannot report its pin cannot be verified, so this is a failure, "
                "not a skip."
            )
        return m.group(1)


def verify(binary: Path) -> int:
    """Does the installed binary agree with the source it should have been built from?"""
    want, got = source_pin(), installed_pin(binary)
    print(f"  source declares : {want}")
    print(f"  {binary.name} seeds : {got}")
    if want != got:
        # Flush first: stderr is unbuffered, so without this the diagnosis prints BEFORE the two
        # values it is about, and the reader sees the conclusion with no evidence under it.
        sys.stdout.flush()
        print(
            f"\nSKEW: {binary} was built from a different revision than this checkout.\n"
            f"  Every domain-extraction run on this host will fail closed on an unresolvable pin:\n"
            f"  the workflow drop-in carries {want}, this binary's vault only holds {got}.\n"
            f"  Fix: re-run this script without --check.",
            file=sys.stderr,
        )
        return 1
    print("  OK — the installed binary agrees with this checkout.")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--check", action="store_true", help="verify only; install nothing")
    ap.add_argument("--dest", type=Path, default=None, help="install directory")
    ap.add_argument("--debug", action="store_true", help="install the debug build")
    args = ap.parse_args()

    dest_dir = args.dest or default_dest()
    dest = dest_dir / EXE

    if args.check:
        if not dest.exists():
            print(f"nothing installed at {dest}", file=sys.stderr)
            return 1
        return verify(dest)

    profile = "debug" if args.debug else "release"
    build = ["cargo", "build", "--bin", "wicked-core"] + ([] if args.debug else ["--release"])
    print(f"  building ({profile}) …")
    if subprocess.run(build, cwd=REPO).returncode != 0:
        return 1

    built = REPO / "target" / profile / EXE
    if not built.exists():
        print(f"build reported success but {built} is missing", file=sys.stderr)
        return 1

    dest_dir.mkdir(parents=True, exist_ok=True)
    # Copy to a sibling then replace: an interrupted copy must not leave a truncated binary on PATH,
    # and on Windows the destination cannot be opened for writing while it is running.
    staged = dest.with_name(dest.name + ".new")
    shutil.copy2(built, staged)
    os.replace(staged, dest)
    print(f"  installed -> {dest}")

    if dest_dir not in [Path(p) for p in os.environ.get("PATH", "").split(os.pathsep) if p]:
        print(f"  NOTE: {dest_dir} is not on PATH; the engine resolves `wicked-core` from PATH.")

    return verify(dest)


if __name__ == "__main__":
    sys.exit(main())
