#!/usr/bin/env python3
"""Publish missing npm packages, platforms first and main last (core#1556).

Usage (from crates/wicked-core-ts):
  python3 ../../scripts/npm-publish-resumable.py --main . npm/*/ -- --access public --provenance --tag latest

Validate the complete package set before any npm call. Only a JSON E404 from a
failed probe permits publishing; every other probe failure leaves the package
missing. Exit 1 if any expected package remains missing. Stdlib only.
"""

import argparse
import json
import subprocess
import sys
from pathlib import Path


def read_manifest(directory):
    """Read the identity shared by validation, probes and publishing."""
    with (Path(directory) / "package.json").open(encoding="utf-8") as fh:
        manifest = json.load(fh)
    if not isinstance(manifest, dict):
        raise ValueError("package.json must be an object")
    for key in ("name", "version"):
        if not isinstance(manifest.get(key), str) or not manifest[key]:
            raise ValueError(f"package.json must have a nonempty {key} string")
    return manifest


def validate(main_dir, platform_dirs):
    """Return (main manifest, name -> directory, errors), without running npm."""
    errors = []
    try:
        main = read_manifest(main_dir)
    except (OSError, ValueError, UnicodeError) as exc:
        return None, {}, [f"{main_dir}: cannot read main package: {exc}"]
    optional = main.get("optionalDependencies")
    if not isinstance(optional, dict) or not optional or any(not name for name in optional):
        return main, {}, [f"{main['name']}: optionalDependencies must be a non-empty object with package names"]
    for name, version in optional.items():
        if version != main["version"]:
            errors.append(
                f"{name}: optionalDependency version {version} differs from main version {main['version']}"
            )
    expected = {main["name"], *optional}
    directories = {main["name"]: main_dir}
    for directory in sorted(platform_dirs):
        try:
            manifest = read_manifest(directory)
        except (OSError, ValueError, UnicodeError) as exc:
            errors.append(f"{directory}: cannot read platform package: {exc}")
            continue
        name = manifest["name"]
        if name not in expected:
            errors.append(f"{name}: unexpected package directory {directory}")
        if name in directories:
            errors.append(f"{name}: duplicate package directory {directory}")
        else:
            directories[name] = directory
        if manifest["version"] != main["version"]:
            errors.append(
                f"{name}: version {manifest['version']} differs from main version {main['version']}"
            )
    for name in sorted(expected - directories.keys()):
        errors.append(f"{name}: missing package directory")
    return main, directories, errors


def probe(name, version, runner):
    """Return present, absent or failed; only stdout's JSON can establish absence."""
    command = ["npm", "view", "--json", "--prefer-online", f"{name}@{version}", "version"]
    try:
        result = runner(command, capture_output=True, text=True, timeout=60, check=False)
    except subprocess.TimeoutExpired:
        return "failed", "probe timed out after 60 s"
    except UnicodeError as exc:
        return "failed", f"probe output could not be decoded: {exc}"
    except OSError as exc:
        return "failed", f"probe could not run: {exc}"
    try:
        value = json.loads(result.stdout)
    except (ValueError, TypeError):
        return "failed", f"probe exit code {result.returncode}: stdout is not JSON"
    if result.returncode == 0:
        if isinstance(value, str) and value == version:
            return "present", ""
        return "failed", f"probe exit code 0: unexpected version value {value!r}"
    error = value.get("error") if isinstance(value, dict) else None
    code = error.get("code") if isinstance(error, dict) else None
    if code == "E404":
        return "absent", ""
    return "failed", f"probe exit code {result.returncode}: error code {code!r}"


def main(argv, runner=subprocess.run) -> int:
    # Split explicitly so npm options pass through verbatim, including their order.
    separator = argv.index("--") if "--" in argv else len(argv)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--main", required=True, help="main package directory")
    parser.add_argument("platform_dirs", nargs="*", help="platform package directories")
    args = parser.parse_args(argv[:separator])
    publish_args = argv[separator + 1:]

    manifest, directories, errors = validate(args.main, args.platform_dirs)
    if errors:
        for error in errors:
            print(f"::error::{error}")
        return 1

    main_name, version = manifest["name"], manifest["version"]
    platforms = sorted(
        (name for name in directories if name != main_name), key=directories.get
    )
    accounted = set()
    missing = {}
    published = already = 0
    for name in platforms + [main_name]:
        identity = f"{name}@{version}"
        state, reason = probe(name, version, runner)
        if state == "present":
            print(f"ALREADY PUBLISHED: {identity}; skipping")
            accounted.add(name)
            already += 1
            continue
        if state == "failed":
            missing[name] = reason
            continue
        if name == main_name and any(platform not in accounted for platform in platforms):
            missing[name] = "skipped main publish: platform packages are missing"
            continue
        print(f"==> npm publish {directories[name]} ({identity})", flush=True)
        try:
            result = runner(["npm", "publish", directories[name], *publish_args], check=False)
        except OSError as exc:
            missing[name] = f"publish could not run: {exc}"
            continue
        if result.returncode != 0:
            missing[name] = f"publish exit code {result.returncode}"
            continue
        accounted.add(name)
        published += 1

    for name in platforms + [main_name]:
        if name not in accounted:
            print(f"::error::{name}@{version}: {missing[name]}")
    if missing:
        return 1
    print(f"{published} published now, {already} already on the registry")
    return 0


if __name__ == "__main__":
    # Line-buffer so our lines stay in order with the npm subprocess output in the CI log.
    sys.stdout.reconfigure(line_buffering=True)
    raise SystemExit(main(sys.argv[1:]))
