//! A published-snapshot fixture laid out EXACTLY as crew publishes a generation (core#396; review
//! pass 7), shared by the integration tests that launch runs whose workflows name skills. The
//! plan-wide skills admission runs before a run's FIRST unit; a hermetic runner has no snapshot and
//! no live garden cache, so without one of these every such run is refused by name — and a test
//! that passes only because the developer's machine has garden installed has an ambient dependency
//! it must not have.
//!
//! Layout (crew's `SnapshotManifest`, `packages/crew/src/skills/store.ts`):
//! `<base>/crew-state/skills/snapshots/<gen>/` — `.claude-plugin/plugin.json`, `snapshot.json`
//! (`gen` == the directory as a number, a 64-hex `contentHash`, `gardenSource {kind, path,
//! plugin_version, baseline}`, `venv`, one row per skill with `dir: "skills/<dir>"`, `portable`
//! and `nested`, and the `views` block), and `skills/<dir>/SKILL.md` whose frontmatter `name` is
//! the referenced name. Included from a test binary with
//! `#[path = "support/skills_snapshot_fixture.rs"] mod skills_fixture;`.

use std::path::{Path, PathBuf};

/// The catalog's naming convention: a skill named `wicked-garden-<dir>` lives at `skills/<dir>`
/// (the loader and crew both require the frontmatter name to be the path-derived one).
const PLUGIN_NAME: &str = "wicked-garden";

/// Publish a generation `gen` (a zero-padded decimal directory name, e.g. `000001`) under
/// `<base>/crew-state/skills/snapshots/`, holding one portable top-level skill per NAME in
/// `names` (each `wicked-garden-<dir>`). Returns the generation root — the value
/// `WICKED_SKILLS_SNAPSHOT` takes. `base` must be CANONICAL (the OS temp dir is a symlink on
/// macOS; the loader refuses an ancestor symlink).
pub fn publish_fixture_snapshot(base: &Path, gen: &str, names: &[&str]) -> PathBuf {
    let gen_number: u64 = gen
        .parse()
        .expect("a generation directory is decimal digits");
    let root = base
        .join("crew-state")
        .join("skills")
        .join("snapshots")
        .join(gen);
    std::fs::create_dir_all(root.join(".claude-plugin")).unwrap();
    std::fs::write(
        root.join(".claude-plugin").join("plugin.json"),
        format!("{{\"name\":\"{PLUGIN_NAME}\",\"version\":\"0.0.0-fixture\"}}"),
    )
    .unwrap();
    let mut rows = Vec::with_capacity(names.len());
    for name in names {
        let dir = name
            .strip_prefix(&format!("{PLUGIN_NAME}-"))
            .unwrap_or_else(|| panic!("{name} is not a {PLUGIN_NAME}-<dir> name"));
        let skill = root.join("skills").join(dir);
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: fixture skill for the integration tests\n---\n\n# {name}\n"),
        )
        .unwrap();
        rows.push(serde_json::json!({
            "name": name,
            "dir": format!("skills/{dir}"),
            "kind": "fork-worker",
            "core": false,
            "portable": true,
            "nested": false
        }));
    }
    // A 64-hex content hash of the crew shape (`^[0-9a-f]{64}$`): the fixture is not re-hashed by
    // the engine, so a deterministic digit string keyed by the generation is enough here.
    let content_hash = format!("{gen_number:0>64}");
    std::fs::write(
        root.join("snapshot.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "gen": gen_number,
            "contentHash": content_hash,
            "gardenSource": {
                "kind": "directory",
                "path": "/fixture/wicked-garden",
                "plugin_version": "0.0.0-fixture",
                "baseline": format!("{:0>64}", 1)
            },
            "venv": "skipped",
            "skills": rows,
            "views": { "copilot": { "dir": "views/copilot", "skills": [] } }
        }))
        .unwrap(),
    )
    .unwrap();
    root
}
