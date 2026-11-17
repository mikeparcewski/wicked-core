#!/usr/bin/env python3
"""Print the CHANGELOG.md notes for one wicked-core-ts release (the GitHub Release body, core#1552).

Usage: changelog-section.py <version>   (accepts "0.15.45", "v0.15.45" or the tag "core-ts-v0.15.45")

Two shapes exist in CHANGELOG.md:
  * a release bullet, `- **core-ts X.Y.Z** — <date> — …`, followed by its indented lines (0.7.2 onward); the
    notes are the bullet's text and those lines, dedented by two spaces;
  * a heading, `## [core-ts X.Y.Z] — <date>` (0.6.x / early 0.7.x); the notes are the section body.
Link-reference lines (`[x]: https://…`) are dropped, and machine paths are scrubbed (a macOS or Linux home
directory becomes `~/`, the macOS temp root becomes `<tmp>/`), so the Releases page never shows one.

Exit codes (a caller can tell "no notes" from a real failure):
  0 — notes found and written to stdout
  1 — no block for this version (nothing written; the caller may write a generic note)
  2 — a real error (bad usage, CHANGELOG.md missing or unreadable): the caller must fail, not fall back

CHANGELOG.md is read from the current directory, else from the repository root above scripts/. Stdlib only.
"""

import os
import re
import sys

_VERSION = re.compile(r"^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$")
_LINK_REF = re.compile(r"(?m)^\[[^\]]+\]:\s*https?://\S+\s*$\n?")
_HOME = re.compile(r"/(?:Users|home)/[^/\s]+/")
_TMP = re.compile("/" + "/".join(("private", "tmp")) + "/")


def _changelog_path() -> str:
    if os.path.exists("CHANGELOG.md"):
        return "CHANGELOG.md"
    here = os.path.dirname(os.path.abspath(__file__))
    return os.path.normpath(os.path.join(here, "..", "CHANGELOG.md"))


def scrub(text: str) -> str:
    """Home and temp paths out of the public notes."""
    return _TMP.sub("<tmp>/", _HOME.sub("~/", text))


def section(changelog: str, version: str):
    """The notes for `version`, or None when CHANGELOG.md has no block for it."""
    lines = changelog.splitlines()
    esc = re.escape(version)
    bullet = re.compile(rf"^- \*\*core-ts {esc}\*\*(?=[\s—]|$)")
    # `## [core-ts X.Y.Z] — …`, or a combined one such as `## [0.4.0] / [core-ts 0.4.1]`.
    heading = re.compile(rf"^## (?:.*\s)?\[core-ts {esc}\](?=[\s—]|$)")
    for i, line in enumerate(lines):
        if bullet.match(line):
            body = [line[2:]]
            for nxt in lines[i + 1:]:
                if nxt.strip() and not nxt.startswith(" "):
                    break  # the next column-0 line (another bullet, a heading) ends the block
                body.append(nxt[2:] if nxt.startswith("  ") else nxt.lstrip())
            return "\n".join(body).strip()
        if heading.match(line):
            body = []
            for nxt in lines[i + 1:]:
                if nxt.startswith("## "):
                    break
                body.append(nxt)
            return _LINK_REF.sub("", "\n".join(body)).strip() or f"wicked-core-ts {version}"
    return None


def main(argv) -> int:
    if len(argv) != 1:
        sys.stderr.write("usage: changelog-section.py <version | core-ts-vX.Y.Z>\n")
        return 2
    arg = argv[0]
    version = arg[len("core-ts-"):] if arg.startswith("core-ts-") else arg
    version = version[1:] if version.startswith("v") else version
    if not _VERSION.match(version):
        sys.stderr.write(f"not a version: {arg!r}\n")
        return 2
    try:
        with open(_changelog_path(), encoding="utf-8") as fh:
            text = fh.read()
    except (OSError, UnicodeDecodeError) as exc:
        sys.stderr.write(f"cannot read CHANGELOG.md: {exc}\n")
        return 2
    notes = section(text, version)
    if notes is None:
        return 1
    sys.stdout.write(scrub(_LINK_REF.sub("", notes)).strip() + "\n")
    return 0


def run(argv) -> int:
    """`main`, with any unexpected failure as exit 2: exit 1 means only "no block for this version"."""
    try:
        return main(argv)
    except Exception as exc:  # noqa: BLE001 — the caller must fail, never fall back to a generic note
        sys.stderr.write(f"changelog-section.py failed: {exc.__class__.__name__}: {exc}\n")
        return 2


if __name__ == "__main__":
    raise SystemExit(run(sys.argv[1:]))
