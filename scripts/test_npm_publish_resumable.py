"""Tests for scripts/npm-publish-resumable.py (the resumable core-ts npm publish, core#1556). Stdlib only:
python3 -m unittest discover -s scripts -p 'test_*.py'"""

import importlib.util
import io
import json
import subprocess
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

_SPEC = importlib.util.spec_from_file_location(
    "npm_publish_resumable", Path(__file__).with_name("npm-publish-resumable.py")
)
npr = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(npr)

VERSION = "1.2.3"
MAIN = "test-main"
PLATFORMS = [f"test-platform-{i}" for i in range(5)]
FLAGS = ["--access", "public", "--provenance", "--tag", "release-1.2"]
ABSENT = (1, json.dumps({"error": {"code": "E404"}}))
PRESENT = (0, json.dumps(VERSION))


class FakeRunner:
    def __init__(self, directories, probes=None, publishes=None):
        self.directories = directories
        self.probes = probes or {}
        self.publishes = publishes or {}
        self.calls = []

    def __call__(self, args, **kwargs):
        self.calls.append((args, kwargs))
        if args[:2] == ["npm", "view"]:
            name = args[4].rsplit("@", 1)[0]
            result = self.probes.get(name, ABSENT)
            if isinstance(result, Exception):
                raise result
            # Even an E404 on stderr cannot establish absence.
            return subprocess.CompletedProcess(args, result[0], result[1], ABSENT[1])
        if args[:2] == ["npm", "publish"]:
            name = next(name for name, path in self.directories.items() if str(path) == args[2])
            return subprocess.CompletedProcess(args, self.publishes.get(name, 0))
        raise AssertionError(f"unexpected command: {args}")

    def published_names(self):
        return [
            next(name for name, path in self.directories.items() if str(path) == args[2])
            for args, _ in self.calls if args[:2] == ["npm", "publish"]
        ]


class NpmPublishResumable(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        self.directories = {MAIN: root / "main"}
        self.directories.update({name: root / "npm" / str(i) for i, name in enumerate(PLATFORMS)})
        for name, directory in self.directories.items():
            directory.mkdir(parents=True)
            manifest = {"name": name, "version": VERSION}
            if name == MAIN:
                manifest["optionalDependencies"] = dict.fromkeys(PLATFORMS, VERSION)
            (directory / "package.json").write_text(json.dumps(manifest), encoding="utf-8")

    def run_publish(self, probes=None, publishes=None, platforms=None):
        runner = FakeRunner(self.directories, probes, publishes)
        # Deliberately reverse the input to pin sorted platform traversal.
        paths = platforms if platforms is not None else [self.directories[n] for n in reversed(PLATFORMS)]
        argv = ["--main", str(self.directories[MAIN]), *map(str, paths), "--", *FLAGS]
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            code = npr.main(argv, runner=runner)
        return code, out.getvalue() + err.getvalue(), runner

    def assert_missing(self, output, name, reason):
        errors = [line for line in output.splitlines() if line.startswith("::error::")]
        self.assertTrue(any(name in line and reason in line.lower() for line in errors), output)

    def test_fresh_publishes_all_six_platforms_sorted_main_last(self):
        code, output, runner = self.run_publish()
        self.assertEqual(code, 0, output)
        self.assertEqual(runner.published_names(), PLATFORMS + [MAIN])
        self.assertIn("6 published now", output)
        self.assertIn("0 already", output)
        probes = [(args, kw) for args, kw in runner.calls if args[1] == "view"]
        self.assertEqual([args for args, _ in probes], [
            ["npm", "view", "--json", "--prefer-online", f"{name}@{VERSION}", "version"]
            for name in PLATFORMS + [MAIN]
        ])
        for _, kwargs in probes:
            self.assertEqual(kwargs["timeout"], 60)
            self.assertTrue(kwargs["capture_output"])
            self.assertTrue(kwargs["text"])

    def test_full_rerun_skips_all_six(self):
        code, output, runner = self.run_publish(probes=dict.fromkeys(self.directories, PRESENT))
        self.assertEqual(code, 0, output)
        self.assertEqual(runner.published_names(), [])
        self.assertEqual(len(runner.calls), 6)
        self.assertIn("0 published now", output)
        self.assertIn("6 already", output)
        for name in self.directories:
            self.assertIn(f"{name}@{VERSION}", output)

    def test_partial_rerun_publishes_only_missing_platforms_then_main(self):
        code, output, runner = self.run_publish(probes=dict.fromkeys(PLATFORMS[:3], PRESENT))
        self.assertEqual(code, 0, output)
        self.assertEqual(runner.published_names(), PLATFORMS[3:] + [MAIN])
        self.assertIn("3 published now", output)
        self.assertIn("3 already", output)

    def test_only_nonzero_json_e404_allows_publish(self):
        failures = [
            (1, json.dumps({"error": {"code": code}}))
            for code in ("ETIMEDOUT", "ENOTFOUND", "E500", "E401", "E403")
        ] + [
            (1, "not JSON"), (1, ""), (1, json.dumps(VERSION)),
            (1, json.dumps({"code": "E404"})),
            (1, json.dumps({"error": "E404"})), (1, "null"),
            (0, json.dumps({"error": {"code": "E404"}})),
            (0, json.dumps("9.9.9")), (0, '""'), (0, ""),
            (0, json.dumps([VERSION])), (0, "null"),
            subprocess.TimeoutExpired(["npm", "view"], 60),
            UnicodeDecodeError("utf-8", b"\xff", 0, 1, "invalid output"),
        ]
        failed = PLATFORMS[0]
        for result in failures:
            with self.subTest(result=result):
                code, output, runner = self.run_publish(probes={failed: result})
                self.assertEqual(code, 1, output)
                self.assertEqual(runner.published_names(), PLATFORMS[1:])
                self.assert_missing(output, failed, "probe")
                self.assert_missing(output, MAIN, "skipped")

    def test_platform_publish_failure_continues_and_blocks_main(self):
        failed = PLATFORMS[0]
        code, output, runner = self.run_publish(publishes={failed: 17})
        self.assertEqual(code, 1, output)
        self.assertEqual(runner.published_names(), PLATFORMS)
        self.assert_missing(output, failed, "publish exit code 17")
        self.assert_missing(output, MAIN, "skipped")

    def test_reports_each_missing_platform(self):
        code, output, _ = self.run_publish(
            probes={PLATFORMS[0]: (1, "network error")}, publishes={PLATFORMS[1]: 3}
        )
        self.assertEqual(code, 1, output)
        self.assert_missing(output, PLATFORMS[0], "probe")
        self.assert_missing(output, PLATFORMS[1], "publish exit code 3")
        self.assert_missing(output, MAIN, "skipped")

    def test_main_publish_failure(self):
        code, output, runner = self.run_publish(publishes={MAIN: 9})
        self.assertEqual(code, 1, output)
        self.assertEqual(runner.published_names(), PLATFORMS + [MAIN])
        self.assert_missing(output, MAIN, "publish exit code 9")

    def test_main_probe_failure(self):
        code, output, runner = self.run_publish(probes={MAIN: (1, "network error")})
        self.assertEqual(code, 1, output)
        self.assertEqual(runner.published_names(), PLATFORMS)
        self.assert_missing(output, MAIN, "probe")

    def test_existing_main_does_not_hide_platform_failure(self):
        code, output, runner = self.run_publish(
            probes={MAIN: PRESENT, PLATFORMS[0]: (1, "network error")}
        )
        self.assertEqual(code, 1, output)
        self.assertNotIn(MAIN, runner.published_names())
        self.assert_missing(output, PLATFORMS[0], "probe")

    def test_expected_package_without_directory_fails_before_any_npm(self):
        code, output, runner = self.run_publish(platforms=[self.directories[n] for n in PLATFORMS[1:]])
        self.assertEqual(code, 1, output)
        self.assertEqual(runner.calls, [])
        self.assert_missing(output, PLATFORMS[0], "dir")

    def test_missing_or_empty_optional_dependencies_fails_before_any_npm(self):
        path = self.directories[MAIN] / "package.json"
        for case in ("missing", "empty"):
            with self.subTest(case=case):
                manifest = {"name": MAIN, "version": VERSION}
                if case == "empty":
                    manifest["optionalDependencies"] = {}
                path.write_text(json.dumps(manifest))
                code, output, runner = self.run_publish(platforms=[])
                self.assertEqual(code, 1, output)
                self.assertEqual(runner.calls, [])
                self.assert_missing(output, MAIN, "optionaldependencies must be a non-empty object")

    def test_optional_dependency_version_mismatch_fails_before_any_npm(self):
        failed = PLATFORMS[-1]
        path = self.directories[MAIN] / "package.json"
        manifest = json.loads(path.read_text())
        manifest["optionalDependencies"][failed] = "0.0.1"
        path.write_text(json.dumps(manifest))
        code, output, runner = self.run_publish()
        self.assertEqual(code, 1, output)
        self.assertEqual(runner.calls, [])
        self.assert_missing(
            output, failed, f"optionaldependency version 0.0.1 differs from main version {VERSION}"
        )

    def test_unexpected_directory_fails_before_any_npm(self):
        extra = Path(self.temp.name) / "extra"
        extra.mkdir()
        (extra / "package.json").write_text(json.dumps({"name": "unexpected", "version": VERSION}))
        code, output, runner = self.run_publish(
            platforms=[self.directories[n] for n in PLATFORMS] + [extra]
        )
        self.assertEqual(code, 1, output)
        self.assertEqual(runner.calls, [])
        self.assert_missing(output, "unexpected", "unexpected")

    def test_version_mismatch_fails_before_any_npm(self):
        failed = PLATFORMS[-1]
        path = self.directories[failed] / "package.json"
        manifest = json.loads(path.read_text())
        manifest["version"] = "0.0.1"
        path.write_text(json.dumps(manifest))
        code, output, runner = self.run_publish()
        self.assertEqual(code, 1, output)
        self.assertEqual(runner.calls, [])
        self.assert_missing(output, failed, "version")

    def test_unreadable_manifest_fails_before_any_npm(self):
        (self.directories[PLATFORMS[-1]] / "package.json").unlink()
        code, output, runner = self.run_publish()
        self.assertEqual(code, 1, output)
        self.assertEqual(runner.calls, [])
        self.assertIn("::error::", output)

    def test_passthrough_args_reach_every_publish_verbatim(self):
        code, output, runner = self.run_publish()
        self.assertEqual(code, 0, output)
        publishes = [args for args, _ in runner.calls if args[1] == "publish"]
        self.assertEqual(len(publishes), 6)
        for args in publishes:
            self.assertEqual(args[3:], FLAGS)


if __name__ == "__main__":
    unittest.main()
