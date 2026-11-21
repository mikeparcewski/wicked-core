"""Tests for scripts/core-ts-platform-lock.py (the napi-release lockfile re-pin, core#1551). Stdlib only:
python3 -m unittest discover -s scripts -p 'test_*.py'"""

import copy
import importlib.util
import io
import json
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

_SPEC = importlib.util.spec_from_file_location(
    "core_ts_platform_lock", Path(__file__).with_name("core-ts-platform-lock.py")
)
cpl = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(cpl)

VERSION = "0.15.47"
PLATFORMS = ["wicked-core-ts-darwin-arm64", "wicked-core-ts-linux-x64-gnu"]


def release_pr_lock(version=VERSION):
    """The shape a release PR writes: versioned platform entries, no resolved/integrity."""
    packages = {
        "": {
            "name": "wicked-core-ts",
            "version": version,
            "optionalDependencies": {name: version for name in PLATFORMS},
            "devDependencies": {"@napi-rs/cli": "^3.0.0"},
        },
        "node_modules/@napi-rs/cli": {"version": "3.0.0", "dev": True},
    }
    for name in PLATFORMS:
        packages[f"node_modules/{name}"] = {
            "version": version,
            "cpu": ["x64"],
            "license": "MIT",
            "optional": True,
            "os": ["linux"],
        }
    return {"name": "wicked-core-ts", "version": version, "lockfileVersion": 3, "packages": packages}


def pinned(lock, version=VERSION):
    """What `npm install --package-lock-only` writes once the entries are gone and npm serves them."""
    lock = copy.deepcopy(lock)
    for name in PLATFORMS:
        entry = lock["packages"].setdefault(f"node_modules/{name}", {"optional": True})
        entry["version"] = version
        entry["resolved"] = f"https://registry.npmjs.org/{name}/-/{name}-{version}.tgz"
        entry["integrity"] = "sha512-" + "A" * 86 + "=="
    return lock


class DropTests(unittest.TestCase):
    def test_drop_removes_every_platform_entry_and_keeps_the_rest(self):
        lock = release_pr_lock()
        self.assertEqual(cpl.drop(lock), PLATFORMS)
        for name in PLATFORMS:
            self.assertNotIn(f"node_modules/{name}", lock["packages"])
        # The pins and every other entry stay: npm re-resolves exactly the dropped ones.
        self.assertEqual(lock["packages"][""]["optionalDependencies"], {n: VERSION for n in PLATFORMS})
        self.assertIn("node_modules/@napi-rs/cli", lock["packages"])

    def test_drop_is_idempotent(self):
        lock = release_pr_lock()
        cpl.drop(lock)
        self.assertEqual(cpl.drop(lock), [])

    def test_a_root_without_platform_pins_is_an_error(self):
        lock = release_pr_lock()
        lock["packages"][""]["optionalDependencies"] = {}
        with self.assertRaises(ValueError):
            cpl.drop(lock)


class CheckTests(unittest.TestCase):
    def test_the_release_pr_shape_fails_the_check(self):
        # core#1551: the shape npm left alone, so the pins never landed.
        problems = cpl.check(release_pr_lock(), VERSION)
        self.assertEqual(len(problems), 2 * len(PLATFORMS), problems)
        self.assertTrue(any("resolved None" in p for p in problems))
        self.assertTrue(any("integrity None" in p for p in problems))

    def test_complete_pins_pass(self):
        self.assertEqual(cpl.check(pinned(release_pr_lock()), VERSION), [])

    def test_a_missing_entry_a_stale_version_and_a_foreign_url_are_each_named(self):
        lock = pinned(release_pr_lock())
        del lock["packages"][f"node_modules/{PLATFORMS[0]}"]
        entry = lock["packages"][f"node_modules/{PLATFORMS[1]}"]
        entry["version"] = "0.15.46"
        entry["resolved"] = "https://example.invalid/x.tgz"
        entry["integrity"] = "sha1-abc"
        problems = cpl.check(lock, VERSION)
        self.assertIn(f"{PLATFORMS[0]}: no lock entry", problems)
        self.assertTrue(any(p.startswith(f"{PLATFORMS[1]}: version '0.15.46'") for p in problems))
        self.assertTrue(any(p.startswith(f"{PLATFORMS[1]}: resolved") for p in problems))
        self.assertTrue(any(p.startswith(f"{PLATFORMS[1]}: integrity 'sha1-abc'") for p in problems))


class CliTests(unittest.TestCase):
    def run_main(self, argv):
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            code = cpl.main(argv)
        return code, out.getvalue(), err.getvalue()

    def test_drop_then_check_round_trip_on_disk(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "package-lock.json"
            path.write_text(json.dumps(release_pr_lock(), indent=2) + "\n", encoding="utf-8")
            code, out, _ = self.run_main(["drop", str(path)])
            self.assertEqual(code, 0)
            self.assertIn("dropped 2 platform entries", out)
            text = path.read_text(encoding="utf-8")
            self.assertTrue(text.endswith("}\n"), "npm's trailing newline is kept")
            dropped = json.loads(text)
            self.assertNotIn(f"node_modules/{PLATFORMS[0]}", dropped["packages"])
            # The regeneration's job, simulated; then the check passes.
            path.write_text(json.dumps(pinned(dropped), indent=2) + "\n", encoding="utf-8")
            self.assertEqual(self.run_main(["check", str(path), VERSION])[0], 0)

    def test_check_exits_1_and_names_each_miss(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "package-lock.json"
            path.write_text(json.dumps(release_pr_lock()), encoding="utf-8")
            code, _, err = self.run_main(["check", str(path), VERSION])
            self.assertEqual(code, 1)
            self.assertEqual(err.count("::error::"), 2 * len(PLATFORMS))

    def test_bad_usage_and_an_unreadable_lock_exit_2(self):
        self.assertEqual(self.run_main([])[0], 2)
        self.assertEqual(self.run_main(["check", "x"])[0], 2)
        self.assertEqual(self.run_main(["drop", "/nonexistent/package-lock.json"])[0], 2)


if __name__ == "__main__":
    unittest.main()
