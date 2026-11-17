"""Tests for scripts/changelog-section.py (the GitHub Release notes, core#1552). Stdlib only:
python3 -m unittest discover -s scripts -p 'test_*.py'"""

import importlib.util
import io
import os
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

_SPEC = importlib.util.spec_from_file_location("changelog_section", Path(__file__).with_name("changelog-section.py"))
cs = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(cs)

HOME_DIR = "/" + "/".join(("Users", "someone")) + "/"
TMP_DIR = "/" + "/".join(("private", "tmp")) + "/"

CHANGELOG = """# Changelog

## [Unreleased]

### Changed

- **core#1 — an engine change.** Not a release.
- **core-ts 0.15.45** — 2026-11-17 — patch release, carrying #1547 and #1548.
  - **Judge independence (#1547).** Seen at the gateway; log at HOME_DIR.wicked/x.log.
  - **Known limits.** core#1546; scratch under TMP_DIRrun-1.
- **core-ts 0.15.4** — 2026-10-19 — patch release carrying #1041.
  - **A claude sign-in (#1041).** One paste.
- **core#2 — another change.**

## [core-ts 0.7.1] — 2026-08-25

### Added

- **Project-scoped graph vouching** (#301).

[core-ts 0.7.1]: https://github.com/mikeparcewski/wicked-core/releases/tag/core-ts-v0.7.1

## [core-ts 0.7.0] — 2026-08-19

- First.
""".replace("HOME_DIR", HOME_DIR).replace("TMP_DIR", TMP_DIR)


def run(args, text=CHANGELOG):
    with tempfile.TemporaryDirectory() as d:
        if text is not None:
            Path(d, "CHANGELOG.md").write_text(text, encoding="utf-8")
        cwd = os.getcwd()
        os.chdir(d)
        out, err = io.StringIO(), io.StringIO()
        try:
            with redirect_stdout(out), redirect_stderr(err):
                code = cs.main(args)
        finally:
            os.chdir(cwd)
    return code, out.getvalue()


class ChangelogSection(unittest.TestCase):
    def test_a_release_bullet_is_its_line_and_indented_lines_dedented(self):
        code, out = run(["core-ts-v0.15.45"])
        self.assertEqual(code, 0)
        self.assertTrue(out.startswith("**core-ts 0.15.45** — 2026-11-17"))
        self.assertIn("\n- **Judge independence (#1547).**", out)
        self.assertNotIn("0.15.4**", out)
        self.assertNotIn("core#2", out)

    def test_a_prefix_version_never_matches_a_longer_one(self):
        code, out = run(["0.15.4"])
        self.assertEqual(code, 0)
        self.assertTrue(out.startswith("**core-ts 0.15.4** — 2026-10-19"))
        self.assertNotIn("0.15.45", out)
        self.assertEqual(run(["0.15"])[0], 2)  # not a version

    def test_an_old_heading_section_without_its_link_ref(self):
        code, out = run(["v0.7.1"])
        self.assertEqual(code, 0)
        self.assertIn("Project-scoped graph vouching", out)
        self.assertNotIn("https://", out)
        self.assertNotIn("First.", out)

    def test_home_and_temp_paths_are_scrubbed(self):
        code, out = run(["0.15.45"])
        self.assertNotIn(HOME_DIR, out)
        self.assertNotIn(TMP_DIR, out)
        self.assertIn("~/.wicked/x.log", out)
        self.assertIn("<tmp>/run-1", out)

    def test_no_block_is_exit_1_with_nothing_written(self):
        self.assertEqual(run(["0.9.9"]), (1, ""))

    def test_a_missing_changelog_or_bad_usage_is_exit_2(self):
        from unittest import mock
        with mock.patch.object(cs, "_changelog_path", return_value=os.path.join(tempfile.gettempdir(), "no-such", "CHANGELOG.md")):
            self.assertEqual(run(["0.15.45"], text=None)[0], 2)
        self.assertEqual(run([])[0], 2)
        self.assertEqual(run(["core-ts-vlatest"])[0], 2)

    def test_the_real_changelog_has_the_latest_release(self):
        real = Path(__file__).resolve().parent.parent / "CHANGELOG.md"
        if not real.exists():
            self.skipTest("no CHANGELOG.md beside scripts/")
        notes = cs.section(real.read_text(encoding="utf-8"), "0.15.45")
        self.assertIsNotNone(notes)
        self.assertTrue(notes.startswith("**core-ts 0.15.45**"))


if __name__ == "__main__":
    unittest.main()
