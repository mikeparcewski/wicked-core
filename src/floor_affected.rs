//! FLOOR AFFECTED SET (DES-FLOOR-V2 §5) — which cargo packages a change reaches, for the floor's
//! derived test set ([`crate::repo_checks`]'s `derive_targeted`): the members owning a touched file,
//! the members a manifest change reaches (`cargo metadata --no-deps` on head and base, diffed —
//! run INSIDE the floor's boundary, the manifests and `.cargo/config` being repo-controlled), the
//! members a `Cargo.lock` change reaches (the lock graph's reverse edges), all closed over the
//! reverse path dependencies, and run in one `-p a -p b` invocation. Anything that cannot be
//! computed runs the full suite, said in the check's `source`. The full suite on every OS is the
//! PR's CI.

use std::path::Path;

use crate::floor_runner::cargo_test_sub;
use crate::repo_checks::{
    package_relative, remove_base_export, run_one, touched_files, BaseTree, CheckScratch,
    FloorContext, RepoCheck, Tree, DERIVED_TARGETED_SOURCE,
};
use crate::validator::WorkerSandbox;

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

/// One workspace member as `cargo metadata --no-deps` reports it.
#[derive(Debug, Clone)]
struct Member {
    name: String,
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
fn workspace_members(
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

/// The workspace members a lockfile change reaches: every head entry that is new or changed
/// (version, source, checksum or dependencies), closed over the head lock graph's reverse edges
/// up to the entries with no `source` (the workspace's own packages).
fn lock_reach(base: &str, head: &str) -> Option<Vec<String>> {
    let (b, h) = (lock_entries(base)?, lock_entries(head)?);
    let mut frontier: Vec<String> = h
        .iter()
        .filter(|(k, v)| b.get(*k) != Some(*v))
        .map(|(k, _)| k.clone())
        .collect();
    // A dependency is `name`, `name version` or `name version (source)`; a key is
    // `name version (source)`.
    let dep_matches = |dep: &str, key: &str| {
        let (mut d, mut k) = (dep.split(' '), key.split(' '));
        d.next() == k.next() && d.next().is_none_or(|v| Some(v) == k.next())
    };
    let mut seen: std::collections::BTreeSet<String> = frontier.iter().cloned().collect();
    while let Some(k) = frontier.pop() {
        for (pk, pv) in &h {
            let deps = pv.get("dependencies").and_then(|d| d.as_array());
            let hits = deps
                .into_iter()
                .flatten()
                .filter_map(|d| d.as_str())
                .any(|d| dep_matches(d, &k));
            if hits && seen.insert(pk.clone()) {
                frontier.push(pk.clone());
            }
        }
    }
    Some(
        seen.iter()
            .filter_map(|k| h.get(k))
            .filter(|v| v.get("source").is_none())
            .filter_map(|v| v.get("name")?.as_str().map(String::from))
            .collect(),
    )
}

/// (DES-FLOOR-V2 §5) The packages a derived cargo test set runs: the members owning a touched
/// file, plus the members whose metadata entry changed between the base and the head (a manifest-
/// only change) and the members a lockfile change reaches — closed over the reverse path
/// dependencies. `Err` names why the set could not be computed (the caller runs the full suite).
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
    for f in &files {
        let name = f.rsplit('/').next().unwrap_or(f);
        if f.starts_with(".cargo/") || f.contains("/.cargo/") || name.starts_with("rust-toolchain")
        {
            return Err(format!("`{f}` configures cargo outside package metadata"));
        }
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
            changed.extend(lock_reach(&old, &new).ok_or("a `Cargo.lock` does not parse")?);
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
        let base_members = workspace_members(&tree.dir, check, sandbox, scratch, Tree::Base);
        remove_base_export(worktree, scratch)?;
        let base_members = base_members?;
        // Dependencies whose declaration changed in some member (added, removed, features,
        // default features, version, kind): cargo unifies a dependency's features across every
        // member built together, so EVERY member that depends on one of them is affected, path
        // dependency or not.
        let deps = |e: &serde_json::Value| -> Vec<(String, serde_json::Value)> {
            e.get("dependencies")
                .and_then(|d| d.as_array())
                .into_iter()
                .flatten()
                .filter_map(|d| Some((d.get("name")?.as_str()?.to_string(), d.clone())))
                .collect()
        };
        let mut moved_deps: std::collections::BTreeSet<String> = Default::default();
        // A member removed from the workspace no longer unifies anything it depended on.
        for b in base_members
            .iter()
            .filter(|b| !head.iter().any(|m| m.name == b.name))
        {
            moved_deps.extend(deps(&b.entry).into_iter().map(|(n, _)| n));
        }
        for m in &head {
            let base = base_members.iter().find(|b| b.name == m.name);
            // A changed `[features]` table can turn on any dependency's features — through
            // `dep/feat`, an implicit optional-dependency feature, another feature, a rename — so
            // EVERY dependency of the member moves (a superset, never a resolver emulation).
            if let Some(b) = base.filter(|b| b.entry.get("features") != m.entry.get("features")) {
                moved_deps.extend(deps(&b.entry).into_iter().map(|(n, _)| n));
                moved_deps.extend(deps(&m.entry).into_iter().map(|(n, _)| n));
            }
            if base.map(|b| &b.entry) != Some(&m.entry) {
                changed.insert(m.name.clone());
                let (old, new) = (
                    base.map(|b| deps(&b.entry)).unwrap_or_default(),
                    deps(&m.entry),
                );
                for (name, d) in old.iter().chain(new.iter()) {
                    let in_old = old.iter().any(|(n, o)| n == name && o == d);
                    let in_new = new.iter().any(|(n, o)| n == name && o == d);
                    if in_old != in_new {
                        moved_deps.insert(name.clone());
                    }
                }
            }
        }
        for m in &head {
            // A member whose features moved is built and tested with them; so is every member
            // that depends on it.
            if moved_deps.contains(&m.name)
                || deps(&m.entry).iter().any(|(n, _)| moved_deps.contains(n))
            {
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
        if c.source != DERIVED_TARGETED_SOURCE || cargo_test_sub(&c.argv).is_none() {
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
                    "{DERIVED_TARGETED_SOURCE}; no package is affected by name — the full suite"
                );
            }
            Err(why) => {
                eprintln!(
                    "wicked-core: repo checks floor — the affected set: {why}; the full suite runs"
                );
                c.argv = full;
                c.source = format!(
                    "{DERIVED_TARGETED_SOURCE}; the affected set could not be computed ({why}) — the full suite"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    /// A lockfile change reaches the members whose locked graph it changed — pure.
    #[test]
    fn a_lockfile_change_reaches_only_the_members_that_lock_it() {
        let lock = |serde_v: &str| {
            format!(
                "version = 4\n\n[[package]]\nname = \"a\"\nversion = \"0.1.0\"\ndependencies = [\"serde\"]\n\n\
                 [[package]]\nname = \"b\"\nversion = \"0.1.0\"\ndependencies = [\"a\"]\n\n\
                 [[package]]\nname = \"c\"\nversion = \"0.1.0\"\n\n\
                 [[package]]\nname = \"serde\"\nversion = \"{serde_v}\"\nsource = \"registry+https://example.invalid\"\n"
            )
        };
        let mut hit = lock_reach(&lock("1.0.0"), &lock("1.0.1")).unwrap();
        hit.sort();
        assert_eq!(hit, s(&["a", "b"]));
        assert_eq!(
            lock_reach(&lock("1.0.0"), &lock("1.0.0")).unwrap(),
            Vec::<String>::new()
        );
        assert!(lock_reach("not toml [", &lock("1.0.0")).is_none());
    }
}
