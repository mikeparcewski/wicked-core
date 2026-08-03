//! Cross-artifact constants, pinned by tests that read both artifacts.
//!
//! # The pattern this deletes
//!
//! A value has to be identical in two files. The files are different formats, owned by different
//! parts of the build, and nothing connects them — so the author writes a comment:
//!
//! ```yaml
//! # Keep in lockstep with napi-release.yml's WICKED_ESTATE_REF.
//! WICKED_ESTATE_REF: v0.13.2
//! ```
//!
//! The comment is not a mechanism. It is visible only to someone editing *this* file, and the way
//! these drift is that someone edits the *other* one. Every instance found in this workspace had a
//! correct, well-written comment, and the comment had never once been the thing that caught a change.
//!
//! # The rule
//!
//! **A value that must be identical in two artifacts is either derived from one source, or asserted
//! equal by a test that reads both.** A comment is neither, and adding one is not a fix.
//!
//! Deriving is better where the build allows it. Where it does not — a YAML `env:` block cannot read
//! a Rust `const`, and `Cargo.toml` cannot read `package.json` — the assertion goes here. This module
//! is the home for those: new cross-artifact constant, new `#[test]`, not a new comment.
//!
//! # Why the tests read files rather than pinning literals
//!
//! Each test names one value and locates it in each artifact by key, then asserts the values agree —
//! it never states what the value *should be*. Pinning the literal (`assert_eq!(reference, "v0.13.2")`)
//! would turn every legitimate bump into a test edit, and a test that must be edited on every routine
//! change gets edited without being read. These tests stay silent through a correct bump and fail only
//! on a partial one, which is the only event worth interrupting anybody for.
//!
//! # Scope
//!
//! In-repo pairs only. The same pattern spans repos — wicked-core's bus TTL defaults must equal
//! wicked-bus `lib/config.js`'s, and crew re-spells core's artifact paths in five places (core#170) —
//! and no test in this repo can read a file that is not in it. Those need a shared artifact or a
//! derived value, not an assertion; they are tracked separately.

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    /// Read a repo-relative artifact, panicking with the full path if it is missing.
    ///
    /// Deliberately not `Option`: if one of these files is renamed, a test that quietly skips is a
    /// test that reports success for a pin it no longer checks. Loud is correct here — the fix is to
    /// update the path, which takes a second and is exactly the moment to notice the pin exists.
    fn read(rel: &str) -> String {
        let path = repo_root().join(rel);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("lockstep pin reads {} — {e}", path.display()))
    }

    /// The value of a top-level-ish YAML scalar `key: value`, first occurrence.
    ///
    /// Comment lines are skipped: the workflows *discuss* these keys in prose right above setting
    /// them, and a scanner that matched the prose would compare a sentence against a version string.
    /// Trailing `# ...` comments on the value line are stripped for the same reason.
    fn yaml_scalar(text: &str, key: &str) -> Option<String> {
        let needle = format!("{key}:");
        text.lines()
            .map(str::trim)
            .filter(|l| !l.starts_with('#'))
            .find_map(|l| l.strip_prefix(&needle))
            .map(|v| {
                let v = v.split('#').next().unwrap_or(v);
                v.trim().trim_matches(['"', '\'']).to_string()
            })
    }

    /// Every value of a repeated YAML list entry `- key: value` (a build matrix leg).
    fn yaml_list_values(text: &str, key: &str) -> BTreeSet<String> {
        let needle = format!("- {key}:");
        text.lines()
            .map(str::trim)
            .filter(|l| !l.starts_with('#'))
            .filter_map(|l| l.strip_prefix(&needle))
            .map(|v| {
                let v = v.split('#').next().unwrap_or(v);
                v.trim().trim_matches(['"', '\'']).to_string()
            })
            .collect()
    }

    /// The `version` of the `[package]` section of a Cargo manifest.
    ///
    /// Scoped to `[package]` on purpose — `version` also appears under `[dependencies]`, and the
    /// first match in the file is only the package's own by convention.
    fn cargo_package_version(manifest: &Path) -> String {
        let text = std::fs::read_to_string(manifest)
            .unwrap_or_else(|e| panic!("lockstep pin reads {} — {e}", manifest.display()));
        let mut in_package = false;
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') {
                in_package = line == "[package]";
                continue;
            }
            if !in_package {
                continue;
            }
            if let Some(v) = line.strip_prefix("version") {
                return v
                    .trim_start()
                    .strip_prefix('=')
                    .unwrap_or(v)
                    .trim()
                    .trim_matches('"')
                    .to_string();
            }
        }
        panic!("no [package] version in {}", manifest.display())
    }

    fn core_ts_package_json() -> serde_json::Value {
        let raw = read("crates/wicked-core-ts/package.json");
        serde_json::from_str(&raw).expect("crates/wicked-core-ts/package.json is valid JSON")
    }

    /// CI compiles against one wicked-estate checkout; the napi release build compiles against
    /// another. Both are pinned by a `WICKED_ESTATE_REF` in their own workflow, and the only thing
    /// holding them together today is a comment in each asking the reader to remember the other.
    ///
    /// Divergence is not a build break — both files are individually valid and both jobs go green.
    /// It means the `.node` shipped to npm statically bundles a different estate than the one every
    /// test ran against, so the addon's behaviour in the field is untested by construction. That is
    /// the worst kind of skew: silent, and detectable only in production.
    #[test]
    fn ci_and_the_napi_release_build_against_the_same_estate() {
        const KEY: &str = "WICKED_ESTATE_REF";
        let ci = yaml_scalar(&read(".github/workflows/ci.yml"), KEY)
            .unwrap_or_else(|| panic!("no `{KEY}:` in ci.yml"));
        let release = yaml_scalar(&read(".github/workflows/napi-release.yml"), KEY)
            .unwrap_or_else(|| panic!("no `{KEY}:` in napi-release.yml"));

        assert_eq!(
            ci, release,
            "ci.yml pins {KEY}={ci} but napi-release.yml pins {KEY}={release}. The published \
             addon would bundle an estate no test in this repo ever exercised. Bump both, or \
             neither."
        );
    }

    /// The addon's version is written in three places: the crate manifest napi compiles, the
    /// package.json npm publishes, and one entry per platform in `optionalDependencies` — the
    /// per-target packages that carry the actual `.node`.
    ///
    /// npm resolves those optional deps at their pinned version, not the parent's. Publish a `0.3.1`
    /// shim whose optionalDependencies still say `0.3.0` and `npm install` succeeds, downloads the
    /// previous release's binary, and the caller runs new JS against an old addon. There is no error
    /// at install time and no error at load time — only whatever the ABI mismatch does later.
    #[test]
    fn the_addon_ships_one_version_everywhere_it_is_written() {
        let crate_version =
            cargo_package_version(&repo_root().join("crates/wicked-core-ts/Cargo.toml"));
        let pkg = core_ts_package_json();
        let pkg_version = pkg["version"].as_str().expect("package.json has a version");

        assert_eq!(
            crate_version, pkg_version,
            "Cargo.toml is {crate_version} and package.json is {pkg_version}. napi builds the addon \
             from the crate and publishes it under the package version; they name the same artifact."
        );

        let optional = pkg["optionalDependencies"]
            .as_object()
            .expect("package.json has optionalDependencies");
        assert!(
            !optional.is_empty(),
            "no optionalDependencies — the platform packages that carry the .node are how the addon \
             is delivered, and this pin cannot check a list that is not there"
        );
        for (name, version) in optional {
            assert_eq!(
                version.as_str(),
                Some(pkg_version),
                "optionalDependencies[{name}] is {version} but the package is {pkg_version}; npm \
                 would install that platform's PREVIOUS binary alongside the new JS"
            );
        }
    }

    /// The set of targets the addon supports is written three times: the napi `triples` the CLI
    /// assembles platform packages from, the `optionalDependencies` npm installs them by, and the
    /// release workflow's build matrix that produces the `.node` for each.
    ///
    /// Drop a target from the matrix but leave it in the other two and the release still succeeds:
    /// npm gets a platform package for a binary that was never built. Users on that platform install
    /// cleanly and fail at `require`. Add one to the matrix only, and it is built and thrown away.
    #[test]
    fn every_supported_target_is_built_published_and_installable() {
        /// napi's triple → platform-package suffix convention, spelled once.
        ///
        /// This mapping is the one thing here that IS a hard-coded correspondence, because it is
        /// npm's and napi's, not ours — `x86_64-pc-windows-msvc` is `win32-x64-msvc` no matter what
        /// this repo does. An unmapped triple fails loudly below rather than being skipped.
        const SUFFIX: &[(&str, &str)] = &[
            ("x86_64-apple-darwin", "darwin-x64"),
            ("aarch64-apple-darwin", "darwin-arm64"),
            ("x86_64-unknown-linux-gnu", "linux-x64-gnu"),
            ("aarch64-unknown-linux-gnu", "linux-arm64-gnu"),
            ("x86_64-pc-windows-msvc", "win32-x64-msvc"),
        ];

        let pkg = core_ts_package_json();
        let pkg_name = pkg["name"].as_str().expect("package.json has a name");

        let triples: BTreeSet<String> = pkg["napi"]["triples"]["additional"]
            .as_array()
            .expect("package.json napi.triples.additional")
            .iter()
            .map(|t| t.as_str().expect("a triple is a string").to_string())
            .collect();
        assert!(!triples.is_empty(), "napi.triples.additional is empty");

        let matrix = yaml_list_values(&read(".github/workflows/napi-release.yml"), "target");
        assert_eq!(
            triples,
            matrix,
            "napi.triples.additional and the napi-release.yml build matrix disagree.\n  \
             declared but never built: {:?}\n  built but never published: {:?}",
            triples.difference(&matrix).collect::<Vec<_>>(),
            matrix.difference(&triples).collect::<Vec<_>>()
        );

        let optional: BTreeSet<String> = pkg["optionalDependencies"]
            .as_object()
            .expect("package.json has optionalDependencies")
            .keys()
            .cloned()
            .collect();
        let expected: BTreeSet<String> = triples
            .iter()
            .map(|t| {
                let suffix = SUFFIX
                    .iter()
                    .find(|(triple, _)| triple == t)
                    .unwrap_or_else(|| {
                        panic!(
                            "triple {t} has no npm platform suffix in this test's mapping — add it \
                             (see napi's `platform` naming) so the pin covers the new target"
                        )
                    })
                    .1;
                format!("{pkg_name}-{suffix}")
            })
            .collect();
        assert_eq!(
            optional,
            expected,
            "optionalDependencies does not match the supported triples.\n  \
             missing (built, but npm cannot install it): {:?}\n  \
             extra (npm installs it, but nothing builds it): {:?}",
            expected.difference(&optional).collect::<Vec<_>>(),
            optional.difference(&expected).collect::<Vec<_>>()
        );
    }
}
