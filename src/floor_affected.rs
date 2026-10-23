//! FLOOR AFFECTED SET (DES-FLOOR-V2 §5) — which cargo packages a change reaches, for the floor's
//! derived test set ([`crate::repo_checks`]'s `derive_targeted`): the members owning a touched file,
//! the members a manifest change reaches (`cargo metadata --no-deps` on head and base, diffed —
//! run INSIDE the floor's boundary, the manifests and `.cargo/config` being repo-controlled), every
//! member that reaches a package a manifest or `Cargo.lock` change moved (feature unification,
//! closed over the members' metadata and the lock graphs), all closed over the reverse path
//! dependencies, and run in one `-p a -p b` invocation. Anything that cannot be
//! computed runs the full suite, said in the check's `source`. The full suite on every OS is the
//! PR's CI.

use std::path::Path;

use crate::floor_runner::cargo_test_sub;
use crate::repo_checks::{
    package_relative, remove_base_export, run_one, touched_files, BaseTree, CheckScratch,
    FloorContext, RepoCheck, Tree,
};
use crate::validator::WorkerSandbox;

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

/// One workspace member as `cargo metadata --no-deps` reports it.
#[derive(Debug, Clone)]
pub(crate) struct Member {
    pub(crate) name: String,
    /// The member's directory relative to the check's directory (`/` separated, `""` = the root).
    dir: String,
    /// The directories (same base) of the members it depends on by path.
    path_deps: Vec<String>,
    /// The member's metadata entry with every absolute path made relative — what a manifest-only
    /// change is diffed on.
    entry: serde_json::Value,
}

/// The workspace members of the tree at `dir` (`check.dir` under `tree_root`), from
/// `cargo metadata --format-version 1 --no-deps --offline` run INSIDE the floor's boundary (the
/// manifests and `.cargo/config` are repo-controlled).
pub(crate) fn workspace_members(
    tree_root: &Path,
    check: &RepoCheck,
    sandbox: &WorkerSandbox,
    scratch: &CheckScratch,
    tree: Tree,
) -> Result<Vec<Member>, String> {
    let sub = cargo_test_sub(&check.argv).ok_or("not a cargo test check")?;
    let mut argv = check.argv[..sub].to_vec();
    argv.extend(s(&[
        "metadata",
        "--format-version",
        "1",
        "--no-deps",
        "--offline",
    ]));
    let probe = RepoCheck {
        argv,
        source: "the workspace's metadata (DES-FLOOR-V2)".into(),
        timeout_s: Some(300),
        ..check.clone()
    };
    let r = run_one(tree_root, &probe, sandbox, scratch, tree);
    if !r.passed() {
        return Err(format!("`cargo metadata` did not pass ({})", r.summary()));
    }
    let raw = r
        .logs
        .first()
        .map(|l| crate::floor_log::text(&scratch.logs, l))
        .unwrap_or_default();
    let json: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("`cargo metadata` is not JSON: {e}"))?;
    let root = json
        .get("workspace_root")
        .and_then(|v| v.as_str())
        .ok_or("`cargo metadata` names no workspace root")?
        .to_string();
    // The check runs in `dir`; the workspace root may be above it (a member checked alone).
    let here = tree_root
        .join(&check.dir)
        .canonicalize()
        .map_err(|e| format!("the check's directory: {e}"))?;
    let rel = |abs: &str| -> Option<String> {
        let p = Path::new(abs);
        let p = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        p.strip_prefix(&here)
            .ok()
            .map(|r| r.to_string_lossy().replace('\\', "/"))
    };
    fn relativize(v: &mut serde_json::Value, root: &str) {
        match v {
            serde_json::Value::String(s) => *s = s.replace(root, ""),
            serde_json::Value::Array(a) => a.iter_mut().for_each(|x| relativize(x, root)),
            serde_json::Value::Object(m) => {
                m.remove("id");
                m.values_mut().for_each(|x| relativize(x, root));
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for p in json
        .get("packages")
        .and_then(|v| v.as_array())
        .ok_or("`cargo metadata` lists no packages")?
    {
        let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let manifest = p
            .get("manifest_path")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let dir = Path::new(manifest)
            .parent()
            .and_then(|d| rel(&d.to_string_lossy()))
            .ok_or_else(|| format!("the member `{name}` is outside the checked tree"))?;
        let path_deps = p
            .get("dependencies")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|d| d.get("path").and_then(|v| v.as_str()))
            .filter_map(rel)
            .collect();
        let mut entry = p.clone();
        relativize(&mut entry, &root);
        out.push(Member {
            name: name.to_string(),
            dir,
            path_deps,
            entry,
        });
    }
    Ok(out)
}

/// The member owning `file` (relative to the check's directory): the deepest member directory
/// at or above it.
fn owner_of<'a>(members: &'a [Member], file: &str) -> Option<&'a Member> {
    members
        .iter()
        .filter(|m| m.dir.is_empty() || file.starts_with(&format!("{}/", m.dir)))
        .max_by_key(|m| m.dir.len())
}

/// The `[[package]]` entries of a `Cargo.lock`, keyed `name version (source)`, with their
/// dependency strings — `None` when it does not parse.
fn lock_entries(raw: &str) -> Option<std::collections::BTreeMap<String, toml::Value>> {
    let v: toml::Value = toml::from_str(raw).ok()?;
    let mut out = std::collections::BTreeMap::new();
    for p in v.get("package")?.as_array()? {
        let name = p.get("name")?.as_str()?;
        let version = p.get("version")?.as_str()?;
        let source = p.get("source").and_then(|s| s.as_str()).unwrap_or("");
        out.insert(format!("{name} {version} ({source})"), p.clone());
    }
    Some(out)
}

/// The package names a lockfile change moved: every head entry that is new or changed (version,
/// source, checksum or dependencies) — `None` when either side does not parse.
fn lock_changed(base: &str, head: &str) -> Option<Vec<String>> {
    let (b, h) = (lock_entries(base)?, lock_entries(head)?);
    Some(
        h.iter()
            .filter(|(k, v)| b.get(*k) != Some(*v))
            .filter_map(|(_, v)| v.get("name")?.as_str().map(String::from))
            .collect(),
    )
}

/// Package name to the names it depends on (every version, kind and target merged: a superset).
type Graph = std::collections::BTreeMap<String, std::collections::BTreeSet<String>>;

/// Add a `Cargo.lock`'s edges to `g` — `None` when it does not parse.
fn add_lock(g: &mut Graph, raw: &str) -> Option<()> {
    for v in lock_entries(raw)?.values() {
        let name = v.get("name")?.as_str()?.to_string();
        let deps = v
            .get("dependencies")
            .and_then(|d| d.as_array())
            .into_iter()
            .flatten()
            .filter_map(|d| d.as_str()?.split(' ').next().map(String::from));
        g.entry(name).or_default().extend(deps);
    }
    Some(())
}

/// `from` and every package it reaches in `g`; `Err` names a reached package `g` does not
/// describe (the closure cannot be established, so the caller runs the full suite).
fn reach(
    g: &Graph,
    from: impl IntoIterator<Item = String>,
) -> Result<std::collections::BTreeSet<String>, String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut frontier: Vec<String> = from.into_iter().collect();
    while let Some(n) = frontier.pop() {
        if seen.contains(&n) {
            continue;
        }
        let next = g.get(&n).ok_or_else(|| {
            format!("no lock graph describes `{n}`, so a feature change cannot be closed over")
        })?;
        frontier.extend(next.iter().cloned());
        seen.insert(n);
    }
    Ok(seen)
}

/// A member's declared dependencies (name, declaration) from its metadata entry.
fn deps(e: &serde_json::Value) -> Vec<(String, serde_json::Value)> {
    e.get("dependencies")
        .and_then(|d| d.as_array())
        .into_iter()
        .flatten()
        .filter_map(|d| Some((d.get("name")?.as_str()?.to_string(), d.clone())))
        .collect()
}

/// The workspace's `Cargo.lock` on the head (the nearest at or above the check's directory) and
/// the base's copy of it, whichever exist.
fn workspace_locks(worktree: &Path, ctx: &FloorContext, check: &RepoCheck) -> Vec<String> {
    let mut rel = check.dir.trim_matches('/').to_string();
    loop {
        let p = if rel.is_empty() {
            "Cargo.lock".to_string()
        } else {
            format!("{rel}/Cargo.lock")
        };
        if let Ok(head) = std::fs::read_to_string(worktree.join(&p)) {
            let mut out = vec![head];
            if let (Some(base), Some(git_dir)) = (ctx.base_head.as_deref(), ctx.git_dir.as_deref())
            {
                let env: [(&str, &Path); 2] = [("GIT_DIR", git_dir), ("GIT_WORK_TREE", worktree)];
                let show = format!("{base}:{p}");
                if let Ok(b) = crate::worktree_guard::git_string(worktree, &["show", &show], &env) {
                    out.push(b);
                }
            }
            return out;
        }
        if rel.is_empty() {
            return Vec::new();
        }
        rel = rel
            .rsplit_once('/')
            .map(|(a, _)| a.to_string())
            .unwrap_or_default();
    }
}

/// (DES-FLOOR-V2 §5) The packages a derived cargo test set runs: the members owning a touched
/// file, plus the members whose metadata entry changed between the base and the head (a manifest-
/// only change) and every member that reaches a package a manifest or lockfile change moved —
/// closed over the reverse path dependencies. `Err` names why the set could not be computed (the caller runs the full suite).
fn affected_packages(
    worktree: &Path,
    ctx: &FloorContext,
    check: &RepoCheck,
    sandbox: &WorkerSandbox,
    scratch: &CheckScratch,
) -> Result<Vec<String>, String> {
    let mut files = touched_files(worktree, ctx)?;
    // cargo and rustup read `.cargo/` and `rust-toolchain*` in every ancestor of the check's
    // directory: one anywhere in the change runs the full suite (checked before the paths are
    // made package-relative, which would drop an ancestor's).
    if let Some(f) = files.iter().find(|f| {
        f.starts_with(".cargo/")
            || f.contains("/.cargo/")
            || f.rsplit('/')
                .next()
                .unwrap_or(f)
                .starts_with("rust-toolchain")
    }) {
        return Err(format!("`{f}` configures cargo outside package metadata"));
    }
    if !check.dir.is_empty() {
        files = package_relative(files, &check.dir);
    }
    let head = workspace_members(worktree, check, sandbox, scratch, Tree::Head)?;
    let mut changed: std::collections::BTreeSet<String> = Default::default();
    let mut manifests = false;
    // The packages a manifest or lockfile change MOVED (their features may differ), and the lock
    // graphs read on the way.
    let mut roots: std::collections::BTreeSet<String> = Default::default();
    let mut locks: Vec<String> = Vec::new();
    let mut base_members: Vec<Member> = Vec::new();
    for f in &files {
        let name = f.rsplit('/').next().unwrap_or(f);
        // A manifest or lockfile path is read by the diffs below AND names its owning member (a
        // fixture manifest under `crates/foo/tests/…` belongs to `foo`).
        if name == "Cargo.toml" || name == "Cargo.lock" {
            if let Some(owner) = owner_of(&head, f) {
                changed.insert(owner.name.clone());
            }
        }
        if name == "Cargo.toml" {
            manifests = true;
        } else if name == "Cargo.lock" {
            let (Some(base), Some(git_dir)) = (ctx.base_head.as_deref(), ctx.git_dir.as_deref())
            else {
                return Err("no base to diff the lockfile against".into());
            };
            let rel = if check.dir.is_empty() {
                f.clone()
            } else {
                format!("{}/{f}", check.dir)
            };
            let env: [(&str, &Path); 2] = [("GIT_DIR", git_dir), ("GIT_WORK_TREE", worktree)];
            let old = crate::worktree_guard::git_string(
                worktree,
                &["show", &format!("{base}:{rel}")],
                &env,
            )
            .map_err(|e| format!("the base's `{rel}`: {e}"))?;
            let new = std::fs::read_to_string(worktree.join(&rel))
                .map_err(|e| format!("the head's `{rel}`: {e}"))?;
            roots.extend(lock_changed(&old, &new).ok_or("a `Cargo.lock` does not parse")?);
            locks.extend([old, new]);
        } else {
            let owner =
                owner_of(&head, f).ok_or_else(|| format!("`{f}` is owned by no package"))?;
            changed.insert(owner.name.clone());
        }
    }
    if manifests {
        // The metadata diff: the base's members, from the base's own manifests, exported for the
        // moment it takes (never beside a head check).
        let (Some(base), Some(git_dir)) = (ctx.base_head.as_deref(), ctx.git_dir.as_deref()) else {
            return Err("no base to diff the manifests against".into());
        };
        // `cargo metadata` does not carry every manifest table: a changed `[profile]`,
        // `[patch]`, `[replace]`, `[workspace]` or `[lints]` reaches every member.
        let env: [(&str, &Path); 2] = [("GIT_DIR", git_dir), ("GIT_WORK_TREE", worktree)];
        for f in files
            .iter()
            .filter(|f| f.rsplit('/').next() == Some("Cargo.toml"))
        {
            let rel = if check.dir.is_empty() {
                f.clone()
            } else {
                format!("{}/{f}", check.dir)
            };
            let old = crate::worktree_guard::git_string(
                worktree,
                &["show", &format!("{base}:{rel}")],
                &env,
            )
            .unwrap_or_default();
            let new = std::fs::read_to_string(worktree.join(&rel)).unwrap_or_default();
            let table = |raw: &str| toml::from_str::<toml::Table>(raw).unwrap_or_default();
            let (old, new) = (table(&old), table(&new));
            for key in [
                "profile",
                "patch",
                "replace",
                "workspace",
                "lints",
                "cargo-features",
            ] {
                if old.get(key) != new.get(key) {
                    return Err(format!(
                        "`{rel}` changes `[{key}]`, which package metadata does not carry"
                    ));
                }
            }
            // The resolver (and the edition that defaults it) decides feature unification for the
            // whole workspace.
            let pkg = |t: &toml::Table, k: &str| t.get("package").and_then(|p| p.get(k)).cloned();
            for key in ["resolver", "edition"] {
                if pkg(&old, key) != pkg(&new, key) {
                    return Err(format!(
                        "`{rel}` changes `package.{key}`, which reaches every member"
                    ));
                }
            }
        }
        let tree = BaseTree::export(worktree, scratch, git_dir, base)?;
        let members = workspace_members(&tree.dir, check, sandbox, scratch, Tree::Base);
        remove_base_export(worktree, scratch)?;
        base_members = members?;
        // A member removed from the workspace no longer unifies anything it depended on.
        for b in base_members
            .iter()
            .filter(|b| !head.iter().any(|m| m.name == b.name))
        {
            roots.extend(deps(&b.entry).into_iter().map(|(n, _)| n));
        }
        for m in &head {
            let base = base_members.iter().find(|b| b.name == m.name);
            // A changed `[features]` table can turn on any dependency's features — through
            // `dep/feat`, an implicit optional-dependency feature, another feature, a rename — so
            // EVERY dependency of the member moves (a superset, never a resolver emulation).
            if let Some(b) = base.filter(|b| b.entry.get("features") != m.entry.get("features")) {
                roots.extend(deps(&b.entry).into_iter().map(|(n, _)| n));
                roots.extend(deps(&m.entry).into_iter().map(|(n, _)| n));
            }
            if base.map(|b| &b.entry) != Some(&m.entry) {
                changed.insert(m.name.clone());
                // A dependency whose declaration changed (added, removed, features, default
                // features, version, kind) moves.
                let (old, new) = (
                    base.map(|b| deps(&b.entry)).unwrap_or_default(),
                    deps(&m.entry),
                );
                for (name, d) in old.iter().chain(new.iter()) {
                    let in_old = old.iter().any(|(n, o)| n == name && o == d);
                    let in_new = new.iter().any(|(n, o)| n == name && o == d);
                    if in_old != in_new {
                        roots.insert(name.clone());
                    }
                }
            }
        }
    }
    // (codex r6) Feature unification crosses the dependency graph: a moved package can turn on
    // features of anything it reaches, and those are on for every member built beside it. Every
    // member that reaches what the moved packages reach is affected — over the members' metadata
    // and the lock graphs (head and base); a reached package neither describes runs the full suite.
    if !roots.is_empty() {
        let mut g = Graph::new();
        for m in head.iter().chain(base_members.iter()) {
            g.entry(m.name.clone())
                .or_default()
                .extend(deps(&m.entry).into_iter().map(|(n, _)| n));
        }
        locks.extend(workspace_locks(worktree, ctx, check));
        for l in &locks {
            add_lock(&mut g, l).ok_or("a `Cargo.lock` does not parse")?;
        }
        let moved = reach(&g, roots.iter().cloned())?;
        for m in &head {
            if !reach(&g, [m.name.clone()])?.is_disjoint(&moved) {
                changed.insert(m.name.clone());
            }
        }
    }
    // Reverse path-dependency closure: whatever depends on a changed member is affected.
    let mut frontier: Vec<String> = changed.iter().cloned().collect();
    while let Some(name) = frontier.pop() {
        let Some(dir) = head.iter().find(|m| m.name == name).map(|m| m.dir.clone()) else {
            continue;
        };
        for m in &head {
            if m.path_deps.contains(&dir) && changed.insert(m.name.clone()) {
                frontier.push(m.name.clone());
            }
        }
    }
    // A lockfile entry that is no longer a member (a package removed) selects nothing.
    Ok(changed
        .into_iter()
        .filter(|n| head.iter().any(|m| &m.name == n))
        .collect())
}

/// Resolve every derived cargo test set to its affected packages, in one `-p a -p b` invocation
/// (features still unify across what is selected). Anything that cannot be computed runs the full
/// suite, said in the check's `source`.
pub(crate) fn resolve_affected(
    worktree: &Path,
    ctx: &FloorContext,
    detected: &mut [RepoCheck],
    sandbox: &WorkerSandbox,
    scratch: &CheckScratch,
) {
    for c in detected.iter_mut() {
        // (codex r7) Typed provenance: a subdirectory package's derived set has a prefixed source.
        if !c.derived || cargo_test_sub(&c.argv).is_none() {
            continue;
        }
        let mut full: Vec<String> = Vec::new();
        let mut it = c.argv.iter();
        while let Some(a) = it.next() {
            if a == "-p" {
                it.next();
            } else {
                full.push(a.clone());
            }
        }
        match affected_packages(worktree, ctx, c, sandbox, scratch) {
            Ok(pkgs) if !pkgs.is_empty() => {
                c.argv = full;
                for p in pkgs {
                    c.argv.push("-p".into());
                    c.argv.push(p);
                }
            }
            Ok(_) => {
                c.argv = full;
                c.source = format!(
                    "{}; no package is affected by name — the full suite",
                    c.source
                );
            }
            Err(why) => {
                eprintln!(
                    "wicked-core: repo checks floor — the affected set: {why}; the full suite runs"
                );
                c.argv = full;
                c.source = format!(
                    "{}; the affected set could not be computed ({why}) — the full suite",
                    c.source
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::repo_checks::tests::{git, git_repo_with_commit, scratch};
    #[cfg(unix)]
    use crate::repo_checks::{
        run_floor, test_seam as seam, FloorStage, RepoChecksReport, DERIVED_TARGETED_SOURCE,
        OUTCOME_NEW_FAILURE,
    };

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    /// (codex r6) A lockfile change moves the entries it changed; a moved package's features
    /// reach everything it depends on, and every member that reaches one of those is affected —
    /// pure, over the lock graph.
    #[test]
    fn a_moved_package_reaches_every_member_that_shares_what_it_reaches() {
        let lock = |s_v: &str| {
            format!(
                "version = 4\n\n[[package]]\nname = \"a\"\nversion = \"0.1.0\"\ndependencies = [\"s\"]\n\n\
                 [[package]]\nname = \"b\"\nversion = \"0.1.0\"\ndependencies = [\"common\"]\n\n\
                 [[package]]\nname = \"c\"\nversion = \"0.1.0\"\n\n\
                 [[package]]\nname = \"common\"\nversion = \"1.0.0\"\nsource = \"registry+https://example.invalid\"\n\n\
                 [[package]]\nname = \"s\"\nversion = \"{s_v}\"\nsource = \"registry+https://example.invalid\"\ndependencies = [\"common 1.0.0\"]\n"
            )
        };
        assert_eq!(
            lock_changed(&lock("1.0.0"), &lock("1.0.1")).unwrap(),
            s(&["s"])
        );
        assert!(lock_changed(&lock("1.0.0"), &lock("1.0.0"))
            .unwrap()
            .is_empty());
        assert!(lock_changed("not toml [", &lock("1.0.0")).is_none());
        let mut g = Graph::new();
        add_lock(&mut g, &lock("1.0.0")).unwrap();
        // `a`'s features move `s` (a → s → common): `b` shares `common`, so it is affected too.
        let moved = reach(&g, s(&["s"])).unwrap();
        let hit: Vec<&str> = ["a", "b", "c"]
            .into_iter()
            .filter(|m| !reach(&g, [m.to_string()]).unwrap().is_disjoint(&moved))
            .collect();
        assert_eq!(hit, ["a", "b"]);
        // A package no graph describes cannot be closed over: the full suite.
        assert!(reach(&g, s(&["ghost"])).is_err());
    }

    /// DONE WHEN 5: a manifest-only change runs the metadata closure — the member whose manifest
    /// changed and the members that depend on it by path — in one `-p` invocation, never the
    /// whole workspace; a source change runs its owner plus its dependents.
    #[cfg(unix)]
    #[test]
    fn a_manifest_only_change_runs_the_metadata_closure_not_the_workspace() {
        let _load = seam::pin(0.5, 4);
        let repo = scratch("affected");
        for (name, deps) in [("a", ""), ("b", "a = { path = \"../a\" }\n"), ("c", "")] {
            std::fs::create_dir_all(repo.join(name).join("src")).unwrap();
            std::fs::write(
                repo.join(name).join("Cargo.toml"),
                format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\n{deps}"),
            )
            .unwrap();
            std::fs::write(
                repo.join(name).join("src/lib.rs"),
                "#[cfg(test)]\nmod t {\n    #[test]\n    fn ok() {}\n}\n",
            )
            .unwrap();
        }
        std::fs::write(
            repo.join("Cargo.toml"),
            "[workspace]\nmembers = [\"a\", \"b\", \"c\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        std::fs::write(repo.join(".gitignore"), "Cargo.lock\ntmp/\ntarget/\n").unwrap();
        let base = git_repo_with_commit(&repo);
        let ctx = FloorContext {
            stage: FloorStage::Creator,
            base_head: Some(base),
            git_dir: Some(repo.join(".git")),
            ..FloorContext::default()
        };
        let targeted = |report: &RepoChecksReport| {
            report
                .checks
                .iter()
                .find(|c| c.name == "test_targeted")
                .map(|c| (c.argv.clone(), c.source.clone()))
                .unwrap_or_else(|| panic!("a derived set ran: {report:#?}"))
        };
        // A manifest-only change to `a` (a feature): `a` and its dependent `b`, never `c`.
        std::fs::write(
            repo.join("a/Cargo.toml"),
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[features]\nextra = []\n[dependencies]\n",
        )
        .unwrap();
        let report = run_floor(&repo, &ctx);
        if report.sandbox_error.is_some() {
            return;
        }
        let (argv, source) = targeted(&report);
        assert_eq!(source, DERIVED_TARGETED_SOURCE, "{argv:?}");
        assert!(argv.ends_with(&s(&["-p", "a", "-p", "b"])), "{argv:?}");
        assert!(
            !argv.iter().any(|x| x == "c" || x == "--workspace"),
            "{argv:?}"
        );
        assert!(report.passed, "{report:#?}");
        // A source change in `a` alone: `a` plus its reverse dependency `b`.
        git(&repo, &["checkout", "--", "a/Cargo.toml"]);
        std::fs::write(
            repo.join("a/src/lib.rs"),
            "pub fn x() {}\n#[cfg(test)]\nmod t {\n    #[test]\n    fn ok() {}\n}\n",
        )
        .unwrap();
        let (argv, _) = targeted(&run_floor(&repo, &ctx));
        assert!(argv.ends_with(&s(&["-p", "a", "-p", "b"])), "{argv:?}");
        // (codex r1) A root `[profile]` change beside a source edit in `a`: package metadata does not
        // carry profiles, so every member is affected — the full suite.
        std::fs::write(
            repo.join("Cargo.toml"),
            "[workspace]\nmembers = [\"a\", \"b\", \"c\"]\nresolver = \"2\"\n[profile.dev]\noverflow-checks = false\n",
        )
        .unwrap();
        let (argv, source) = targeted(&run_floor(&repo, &ctx));
        assert!(!argv.iter().any(|x| x == "-p"), "{argv:?}");
        assert!(
            source.contains("`[profile]`") && source.contains("the full suite"),
            "{source}"
        );
        git(&repo, &["checkout", "--", "Cargo.toml"]);
        // (codex r2) A member's edition (which defaults the resolver) reaches every member.
        std::fs::write(
            repo.join("a/Cargo.toml"),
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[dependencies]\n",
        )
        .unwrap();
        let (argv, source) = targeted(&run_floor(&repo, &ctx));
        assert!(!argv.iter().any(|x| x == "-p"), "{argv:?}");
        assert!(source.contains("package.edition"), "{source}");
        git(&repo, &["checkout", "--", "a/Cargo.toml"]);
        // (agy r2, r5) A nested `.cargo/` config, and a path no package owns (a file any crate may
        // `include!`), each reach every crate: the full suite.
        for (path, why) in [
            (
                "b/.cargo/config.toml",
                "configures cargo outside package metadata",
            ),
            ("README.md", "is owned by no package"),
        ] {
            std::fs::create_dir_all(repo.join(path).parent().unwrap()).unwrap();
            std::fs::write(repo.join(path), "\n").unwrap();
            let (argv, source) = targeted(&run_floor(&repo, &ctx));
            assert!(!argv.iter().any(|x| x == "-p"), "{path}: {argv:?}");
            assert!(source.contains(why), "{path}: {source}");
            std::fs::remove_file(repo.join(path)).unwrap();
        }
        // A change in `c` alone: `c` only.
        git(&repo, &["checkout", "--", "a/src/lib.rs"]);
        std::fs::write(repo.join("c/src/lib.rs"), "pub fn y() {}\n").unwrap();
        let (argv, _) = targeted(&run_floor(&repo, &ctx));
        assert!(
            argv.ends_with(&s(&["-p", "c"])) && !argv.contains(&"a".to_string()),
            "{argv:?}"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// (codex r4) A member's `[features]` change that turns on a shared dependency's feature
    /// reaches every member that depends on it (cargo unifies features across what it builds):
    /// `a` enabling `s/bad` selects `b` (which does not depend on `a`) and `s` itself.
    #[cfg(unix)]
    #[test]
    fn a_feature_change_on_a_shared_dependency_selects_its_other_consumers() {
        let _load = seam::pin(0.5, 4);
        let repo = scratch("features");
        let member = |name: &str, deps: &str, features: &str| {
            std::fs::create_dir_all(repo.join(name).join("src")).unwrap();
            std::fs::write(
                repo.join(name).join("Cargo.toml"),
                format!(
                    "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
                     [features]\n{features}[dependencies]\n{deps}"
                ),
            )
            .unwrap();
            std::fs::write(repo.join(name).join("src/lib.rs"), "\n").unwrap();
        };
        member("s", "", "bad = []\n");
        member("a", "s = { path = \"../s\" }\n", "");
        member("b", "s = { path = \"../s\" }\n", "");
        member("c", "", "");
        std::fs::write(
            repo.join("Cargo.toml"),
            "[workspace]\nmembers = [\"s\", \"a\", \"b\", \"c\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        std::fs::write(repo.join(".gitignore"), "Cargo.lock\ntmp/\n").unwrap();
        let base = git_repo_with_commit(&repo);
        let ctx = FloorContext {
            stage: FloorStage::Creator,
            base_head: Some(base),
            git_dir: Some(repo.join(".git")),
            ..FloorContext::default()
        };
        member("a", "s = { path = \"../s\" }\n", "default = [\"s/bad\"]\n");
        let report = run_floor(&repo, &ctx);
        if report.sandbox_error.is_some() {
            return;
        }
        let c = report
            .checks
            .iter()
            .find(|c| c.name == "test_targeted")
            .unwrap_or_else(|| panic!("a derived set ran: {report:#?}"));
        assert!(
            c.argv.ends_with(&s(&["-p", "a", "-p", "b", "-p", "s"])),
            "{:?}",
            c.argv
        );
        assert!(!c.argv.contains(&"c".to_string()), "{:?}", c.argv);
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// (codex r6) A feature change reaches TRANSITIVELY: `a` turning on `s/bad`, which turns on
    /// `common/bad`, affects `b` — which depends only on `common`.
    #[cfg(unix)]
    #[test]
    fn a_feature_change_reaches_the_consumers_of_what_it_turns_on_transitively() {
        let _load = seam::pin(0.5, 4);
        let repo = scratch("features-deep");
        let member = |name: &str, deps: &str, features: &str| {
            std::fs::create_dir_all(repo.join(name).join("src")).unwrap();
            std::fs::write(
                repo.join(name).join("Cargo.toml"),
                format!(
                    "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
                     [features]\n{features}[dependencies]\n{deps}"
                ),
            )
            .unwrap();
            std::fs::write(repo.join(name).join("src/lib.rs"), "\n").unwrap();
        };
        member("common", "", "bad = []\n");
        member(
            "s",
            "common = { path = \"../common\" }\n",
            "bad = [\"common/bad\"]\n",
        );
        member("a", "s = { path = \"../s\" }\n", "");
        member("b", "common = { path = \"../common\" }\n", "");
        member("c", "", "");
        std::fs::write(
            repo.join("Cargo.toml"),
            "[workspace]\nmembers = [\"common\", \"s\", \"a\", \"b\", \"c\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        std::fs::write(repo.join(".gitignore"), "Cargo.lock\ntmp/\n").unwrap();
        let base = git_repo_with_commit(&repo);
        let ctx = FloorContext {
            stage: FloorStage::Creator,
            base_head: Some(base),
            git_dir: Some(repo.join(".git")),
            ..FloorContext::default()
        };
        member("a", "s = { path = \"../s\" }\n", "default = [\"s/bad\"]\n");
        let report = run_floor(&repo, &ctx);
        if report.sandbox_error.is_some() {
            return;
        }
        let c = report
            .checks
            .iter()
            .find(|c| c.name == "test_targeted")
            .unwrap_or_else(|| panic!("a derived set ran: {report:#?}"));
        for p in ["a", "b", "common", "s"] {
            assert!(c.argv.contains(&p.to_string()), "{p}: {:?}", c.argv);
        }
        assert!(!c.argv.contains(&"c".to_string()), "{:?}", c.argv);
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// (agy r6) A package the change introduces, with a failing test, on a green base: the base
    /// has no such package, so its derived set is a check the base does not declare — a NEW
    /// failure, never "base red, cannot separate".
    #[cfg(unix)]
    #[test]
    fn a_new_package_failing_on_a_green_base_is_a_new_failure() {
        let _load = seam::pin(0.5, 4);
        let repo = scratch("new-package");
        let member = |name: &str, body: &str| {
            std::fs::create_dir_all(repo.join("crates").join(name).join("src")).unwrap();
            std::fs::write(
                repo.join("crates").join(name).join("Cargo.toml"),
                // No `edition` (2015): a new manifest's edition runs the full suite.
                format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n"),
            )
            .unwrap();
            std::fs::write(repo.join("crates").join(name).join("src/lib.rs"), body).unwrap();
        };
        member(
            "a",
            "#[cfg(test)]\nmod t {\n    #[test]\n    fn ok() {}\n}\n",
        );
        std::fs::write(
            repo.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        std::fs::write(repo.join(".gitignore"), "Cargo.lock\ntmp/\ntarget/\n").unwrap();
        let base = git_repo_with_commit(&repo);
        let ctx = FloorContext {
            stage: FloorStage::Creator,
            base_head: Some(base),
            git_dir: Some(repo.join(".git")),
            ..FloorContext::default()
        };
        member(
            "n",
            "#[cfg(test)]\nmod t {\n    #[test]\n    fn broken() {\n        panic!(\"new\")\n    }\n}\n",
        );
        let report = run_floor(&repo, &ctx);
        if report.sandbox_error.is_some() {
            return;
        }
        let c = report
            .checks
            .iter()
            .find(|c| c.name == "test_targeted")
            .unwrap_or_else(|| panic!("a derived set ran: {report:#?}"));
        assert!(c.argv.ends_with(&s(&["-p", "n"])), "{:?}", c.argv);
        let d = c.decision.as_ref().expect("a decision");
        assert_eq!(d.outcome, OUTCOME_NEW_FAILURE, "{d:?}");
        let base_err = c
            .base
            .as_ref()
            .and_then(|b| b.error.clone())
            .unwrap_or_default();
        assert!(
            base_err.contains("declares no `test_targeted` check over `n`"),
            "{base_err}"
        );
        assert!(!report.passed);
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// (codex r7) A cargo workspace in a SUBDIRECTORY (`rust/`, no root manifest): its derived set
    /// is closed over like the root's — `b` depends on `a`, so a change to `a` runs `b` too, and
    /// `b`'s regression denies.
    #[cfg(unix)]
    #[test]
    fn a_nested_workspace_runs_the_dependents_of_its_changed_package() {
        let _load = seam::pin(0.5, 4);
        let repo = scratch("nested-ws");
        let rust = repo.join("rust");
        std::fs::create_dir_all(rust.join("src")).unwrap();
        std::fs::create_dir_all(rust.join("b/src")).unwrap();
        std::fs::write(
            rust.join("Cargo.toml"),
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\nmembers = [\"b\"]\ndefault-members = [\".\", \"b\"]\n",
        )
        .unwrap();
        std::fs::write(rust.join("src/lib.rs"), "pub fn f() -> u32 {\n    1\n}\n").unwrap();
        std::fs::write(
            rust.join("b/Cargo.toml"),
            "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\na = { path = \"..\" }\n",
        )
        .unwrap();
        std::fs::write(
            rust.join("b/src/lib.rs"),
            "#[cfg(test)]\nmod t {\n    #[test]\n    fn one() {\n        assert_eq!(a::f(), 1);\n    }\n}\n",
        )
        .unwrap();
        std::fs::write(repo.join(".gitignore"), "Cargo.lock\ntmp/\ntarget/\n").unwrap();
        let base = git_repo_with_commit(&repo);
        let ctx = FloorContext {
            stage: FloorStage::Verify,
            base_head: Some(base),
            git_dir: Some(repo.join(".git")),
            ..FloorContext::default()
        };
        std::fs::write(rust.join("src/lib.rs"), "pub fn f() -> u32 {\n    2\n}\n").unwrap();
        let report = run_floor(&repo, &ctx);
        if report.sandbox_error.is_some() {
            return;
        }
        let c = report
            .checks
            .iter()
            .find(|c| c.name == "test_targeted" && c.source.starts_with("rust/"))
            .unwrap_or_else(|| panic!("the nested derived set ran: {report:#?}"));
        assert!(
            c.argv.ends_with(&s(&["-p", "a", "-p", "b"])),
            "{:?}",
            c.argv
        );
        assert!(!report.passed, "b's regression denies: {report:#?}");
        let _ = std::fs::remove_dir_all(&repo);
    }
}
