//! Source-scan conformance test for the edge-vocabulary pin (AW-19 / arch-R17; estate ADR-011
//! §edge-vocabulary): no NEW split-brain spelling of the governs edge can land in the
//! wicked-core workspace. The stringly spelling is knowledge-store-only and its one legal mint
//! site lives in wicked-estate-knowledge — NOT in this repo — so any source line here that
//! constructs `EdgeKind::Other(…governs…)` is a defect this test fails loudly on.
//!
//! Allowlisted: `src/edge_vocab.rs` (names the spelling in order to ban it — the runtime lint
//! and its unit tests) and this file itself (defensive; the scan pattern does not match its own
//! regex source, but a future edit must not silently un-guard the guard).

use std::path::{Path, PathBuf};

/// Repo-relative paths (forward slashes) that may name the banned construction.
const ALLOWLIST: &[&str] = &[
    "crates/wicked-governance/src/edge_vocab.rs",
    "crates/wicked-governance/tests/edge_vocab_lint.rs",
];

/// Directory names never descended into.
const SKIP_DIRS: &[&str] = &["target", "node_modules", "dist", "vendor"];

fn walk_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            if SKIP_DIRS.contains(&name.as_ref()) {
                continue;
            }
            walk_rs(&path, out);
        } else if path.extension().is_some_and(|x| x == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_stringly_governs_mint_site_in_the_core_workspace() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root resolves");

    // `Other(` followed by a governs spelling before the call closes — catches the literal
    // (`"governs"`, any casing), the `GOVERNS` const, and `String::from("governs")` forms.
    // `[^)]` keeps the match inside the argument list (a closed call cannot smuggle a later
    // `governs` in).
    let banned = regex::Regex::new(r"(?i)other\s*\([^)]{0,60}governs").expect("static regex");

    let mut files = Vec::new();
    for top in ["src", "crates", "tests"] {
        walk_rs(&root.join(top), &mut files);
    }
    assert!(
        files.len() > 10,
        "the scan found almost no Rust sources — the workspace-root resolution is broken, \
         which would make this lint pass vacuously"
    );

    let mut hits = Vec::new();
    for file in files {
        let rel = file
            .strip_prefix(&root)
            .unwrap_or(&file)
            .to_string_lossy()
            .replace('\\', "/");
        if ALLOWLIST.contains(&rel.as_str()) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue; // non-UTF8 source is not a mint site
        };
        if banned.is_match(&text) {
            for (ix, line) in text.lines().enumerate() {
                if banned.is_match(line) {
                    hits.push(format!("{rel}:{}: {}", ix + 1, line.trim()));
                }
            }
            // A multi-line construction with no single-line match still counts.
            if !hits.iter().any(|h| h.starts_with(&rel)) {
                hits.push(format!("{rel}: (multi-line construction)"));
            }
        }
    }

    assert!(
        hits.is_empty(),
        "split-brain governs spelling(s) found — code-graph targets take the native \
         EdgeKind::Governs, and the stringly spelling belongs to wicked-estate-knowledge only \
         (AW-19 / estate ADR-011 §edge-vocabulary):\n{}",
        hits.join("\n")
    );
}
