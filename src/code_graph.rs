//! CODE GRAPH — operate GRAPH-NATIVELY on a repo's code/domain graph, the wicked-estate substrate the
//! whole methodology spine (recon → review → test), memory cross-edges, and routing are built on.
//!
//! ARCHITECTURE: indexing (the heavy 150+ tree-sitter language extractors) is delegated to the
//! `wicked-estate` indexer as a SUBPROCESS, so the grammars stay OUT of this engine/UI binary. The
//! engine then READS + RANKS the resulting graph with the lean `estate-core` (GraphRead) +
//! `estate-rank` (PageRank) crates it already links. Indexing is a build step; operating on the graph
//! is the runtime — keeping them separate is what lets us be graph-native without bloat.
//!
//! # WHERE A REPO'S GRAPH LIVES (ADR, 2026-08-29)
//!
//! **Estate home by default.** A fresh repo's graph is minted at
//! `<estate_root>/<key>/estate.db`, where `estate_root` is `$WICKED_ESTATE_REPO_GRAPH_ROOT` when
//! set, else `<home>/.wicked-estate/repo-graphs` (`$HOME`, or `$USERPROFILE` on Windows), and
//! `<key>` is [`repo_graph_key`]'s `<repo-dir-name>-<12-hex-of-sha256(canonical-root)>`. The old
//! default — `<repo>/.codegraph/estate.db`, INSIDE the working tree — polluted every checkout it
//! touched: a 185 MB database in the tree of a repo whose owner never asked for one, showing up in
//! `git status` on unignored repos and in every backup/sync tool watching the tree. wicked-crew
//! moved PROJECT graphs out for exactly this reason (crew#330, `graph-paths.ts`); this applies the
//! same posture one level down. A directory per key (rather than `<key>.db` files in one flat
//! folder) keeps the db and its WAL/journal siblings together, so removing a repo's graph is one
//! `rm -rf` that cannot strand a `-wal` describing a database that is gone.
//!
//! **Legacy-first, never a silent migration.** If `<repo>/.codegraph/estate.db` EXISTS, both the
//! read path ([`existing_code_graph`]) and the write path ([`code_graph_path_for_write`]) keep
//! using it. An already-indexed repo must never be re-pointed at an empty database — "nothing
//! found" about a repo full of code is FINDING-069's exact failure, and a resolver that answered
//! "estate home" while 185 MB sat in-tree would reintroduce it wholesale. Migration is MANUAL and
//! operator-driven: move `<repo>/.codegraph/estate.db` to `<estate_root>/<key>/estate.db` (mint
//! the key with [`repo_graph_key`]) and delete `<repo>/.codegraph/`, or simply delete
//! `<repo>/.codegraph/` and re-index.
//!
//! **Per-key sandbox grants.** A governed worker whose graph lives in the estate home is granted
//! read+write on EXACTLY its own `<estate_root>/<key>/` directory (write because opening a
//! WAL-mode SQLite db creates `-wal`/`-shm`/journal files in its directory) — never the whole
//! `repo-graphs` root or the estate home, because every OTHER repo's graph lives one sibling over
//! and a worker must not be able to reach it. Legacy in-tree graphs keep the pre-existing behavior
//! byte for byte: the READ boundary widens to the repo root (the graph is inside it, and its file
//! paths anchor there), and no write root is added. [`classify_code_graph_db_at`] is the one shape
//! recognizer both grants key off. Note the trade the estate home makes: its graphs' file paths
//! still anchor to the repo root the indexer ran over, but the per-key grant does not include that
//! root — a worker reads source from its own worktree instead.
//!
//! **Env override.** `$WICKED_ESTATE_REPO_GRAPH_ROOT` relocates the root wholesale — the escape
//! hatch that lets tests and proof scripts run without touching a developer's real home (the same
//! contract as crew's `WICKED_CREW_PROJECT_GRAPH_ROOT`). Set it to an ABSOLUTE path; the sandbox
//! grants fail closed on relative ones. TH-8's environment manifest should list this variable.

use std::path::{Path, PathBuf};
use std::process::Command;

use std::collections::HashSet;

use wicked_apps_core::{open_store, GraphRead, HardenedCommand, NodeKind};
use wicked_estate_core::{Direction, SymbolId};

/// CALL-SPREAD — the number of DISTINCT files that reference `id`. A LANGUAGE-AGNOSTIC, data-driven
/// ubiquity signal: generic utilities (`as_str`, `default`, `new`, `map`, `join`) are called from a
/// large FRACTION of all files, so PageRank over-ranks them; domain symbols (`recall`, `base_dir`,
/// `from_node`) are called from a few. No hardcoded word list — measured from the parsed edges. (We
/// use spread, not raw in-degree, because shallow indexes collapse out-degree but keep edge files.)
fn caller_spread<S: GraphRead>(store: &S, id: &SymbolId) -> usize {
    store
        .neighbors(id, Direction::Dependents)
        .unwrap_or_default()
        .iter()
        .filter_map(|e| e.location.as_ref().map(|l| l.file.clone()))
        .filter(|f| !f.is_empty())
        .collect::<HashSet<_>>()
        .len()
}

/// A ranked code symbol — the orchestrator's recon view of a repo (PageRank centrality over the
/// CALLS/IMPORTS graph). `score_pct` is relative to the top symbol (100 = most central).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct RankedSymbol {
    pub name: String,
    pub file: String,
    pub kind: String,
    pub score_pct: u8,
}

/// Definition kinds that can be a hotspot — substantive, human-authored declarations. Excludes
/// imports/files/modules/namespaces (structural) AND fields/constants/variables/parameters (data,
/// not "load-bearing code") + synthetic nodes. (Mirrors command_iq's function/method/class/interface
/// type filter, generalized.)
fn is_def_kind(k: &NodeKind) -> bool {
    matches!(
        k,
        NodeKind::Class
            | NodeKind::Struct
            | NodeKind::Enum
            | NodeKind::Interface
            | NodeKind::Trait
            | NodeKind::Function
            | NodeKind::Method
            | NodeKind::Constructor
            | NodeKind::TypeAlias
            | NodeKind::Macro
    )
}

/// Files that "sit outside" the human-authored source — tests, generated code, vendored deps. A
/// hotspot must come from the original source, so these are excluded (language-agnostic path match).
fn is_excluded_path(file: &str) -> bool {
    let f = file.to_lowercase();
    const PATS: &[&str] = &[
        "/test/",
        "/tests/",
        "/__tests__/",
        "/spec/",
        "/specs/",
        ".test.",
        ".spec.",
        "_test.",
        "/test_",
        "/node_modules/",
        "/vendor/",
        "/third_party/",
        "/dist/",
        "/build/",
        "/.next/",
        "/__generated__/",
        "/generated/",
        "/migrations/",
        "/target/",
        ".min.",
        ".bundle.",
        "_pb.",
        ".generated.",
        ".g.dart",
        "/gen/",
    ];
    PATS.iter().any(|p| f.contains(p))
}

/// A NodeKind's display label (e.g. "Function", or the inner string for `Other`).
fn kind_str(k: &NodeKind) -> String {
    match k {
        NodeKind::Other(s) => s.clone(),
        other => format!("{other:?}"),
    }
}

/// Resolve the `wicked-estate` indexer binary: `$WICKED_ESTATE_BIN`, then `~/.cargo/bin`, else the
/// bare name (PATH lookup).
pub(crate) fn indexer_bin() -> String {
    if let Ok(b) = std::env::var("WICKED_ESTATE_BIN") {
        if !b.is_empty() {
            return b;
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = Path::new(&home).join(".cargo/bin/wicked-estate");
        if p.exists() {
            return p.display().to_string();
        }
    }
    "wicked-estate".to_string()
}

/// Where a repo's LEGACY in-tree code graph lives, relative to its root. The ONE spelling.
///
/// `.codegraph/estate.db` rather than this engine's own `.wicked/` namespace, because that is the
/// path that has the data. Both spellings existed — the engine indexed to `.wicked/code-graph.db`
/// while crew's onboarding launched `wicked-estate index --db <repo>/.codegraph/estate.db` — and in
/// the deployed topology crew drives onboarding, so on a real repo the 185 MB graph sat at
/// `.codegraph/estate.db` and `.wicked/code-graph.db` did not exist at all (FINDING-069). Picking the
/// engine's spelling would have been the tidier name and would have orphaned every indexed repo.
///
/// No NEW graph is minted here anymore (see the module ADR — fresh repos get the estate home), but
/// a repo that already has this file keeps it, forever, for the same never-orphan reason.
///
/// Written with `/` because that is how every other artifact in the ecosystem spells it — the crew
/// CLI flag, the JS `join`, this doc. Do NOT hand it to [`Path::join`] whole; use
/// [`code_graph_rel`], which is the only correct way to turn it into a path.
pub(crate) const CODE_GRAPH_DB_REL: &str = ".codegraph/estate.db";

/// The filename every code-graph database carries, in BOTH homes (`<repo>/.codegraph/estate.db`
/// and `<estate_root>/<key>/estate.db`).
pub(crate) const CODE_GRAPH_DB_FILE: &str = "estate.db";

/// Env var overriding the estate-home root for per-repo graphs — see [`repo_graph_root`] and the
/// module ADR. TH-8's environment manifest should list it.
pub(crate) const REPO_GRAPH_ROOT_ENV: &str = "WICKED_ESTATE_REPO_GRAPH_ROOT";

/// The estate home every NEW repo graph hangs off: `$WICKED_ESTATE_REPO_GRAPH_ROOT` when set,
/// else `<home>/.wicked-estate/repo-graphs` (home = `$HOME`, or `$USERPROFILE` on Windows).
/// `None` when no home can be resolved at all; the resolver then falls back to the legacy
/// in-tree spelling — the only address left, and the pre-ADR behavior.
pub(crate) fn repo_graph_root() -> Option<PathBuf> {
    repo_graph_root_from(
        std::env::var_os(REPO_GRAPH_ROOT_ENV),
        std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")),
    )
}

/// [`repo_graph_root`]'s pure core, split out so the override precedence is testable without
/// mutating process env (env mutation races parallel tests; the few tests that must mutate hold
/// [`REPO_GRAPH_ROOT_ENV_LOCK`]).
fn repo_graph_root_from(
    override_root: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    if let Some(r) = override_root {
        if !r.is_empty() {
            return Some(PathBuf::from(r));
        }
    }
    home.filter(|h| !h.is_empty())
        .map(|h| Path::new(&h).join(".wicked-estate").join("repo-graphs"))
}

/// Sanitized-stem budget: 51 + `-` + 12 hex = exactly estate's 64-byte label ceiling — the same
/// arithmetic as crew's `graph-paths.ts` (its `HASH_LEN`/`STEM_LEN`).
const KEY_HASH_LEN: usize = 12;
const KEY_STEM_LEN: usize = 64 - 1 - KEY_HASH_LEN;

/// The estate-home directory key for one repo:
/// `<repo-dir-name>-<first 12 hex of sha256(canonicalized absolute repo root)>`.
///
/// The dir name is what lets an operator map a key back to a repo without a lookup table; the
/// digest is what keeps two repos that share a dir name (`~/work/api` and `~/oss/api`) apart. The
/// whole key is sanitized to estate's label charset (`wicked-estate/src/repo_scope.rs::
/// validate_label`: 1–64 chars of `[A-Za-z0-9._-]`, never `.`/`..`, no leading `-`) — the label
/// rule exists because a `/` or `..` in a path segment forges paths in another namespace, and a
/// key IS a path segment here. Illegal characters collapse to `-`, the stem is capped, a leading
/// `-` is trimmed, and an empty stem falls back to `repo`; the digest is always appended, so a
/// sanitized collision still yields distinct keys.
///
/// Canonicalization (falling back to [`std::path::absolute`], then the path as given, for a root
/// that is gone) is what makes the key STABLE across spellings: `/var/...` and `/private/var/...`,
/// a relative registration and its absolute record, all hash to the same key — so the record, the
/// indexer, and the dispatch-time resolver land on the same directory.
pub(crate) fn repo_graph_key(repo: &Path) -> String {
    use sha2::{Digest, Sha256};
    let canon = std::fs::canonicalize(repo)
        .or_else(|_| std::path::absolute(repo))
        .unwrap_or_else(|_| repo.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canon.to_string_lossy().as_bytes());
    let digest: String = hasher
        .finalize()
        .iter()
        .take(KEY_HASH_LEN / 2)
        .map(|b| format!("{b:02x}"))
        .collect();
    // Sanitization maps every non-label char to ASCII `-`, so the stem is pure ASCII and the
    // byte cap below cannot split a char.
    let stem: String = canon
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .take(KEY_STEM_LEN)
        .collect();
    let stem = stem.trim_start_matches('-');
    let stem = if stem.is_empty() { "repo" } else { stem };
    format!("{stem}-{digest}")
}

/// One repo's estate-home graph db under a given root — the estate-home half of the resolver,
/// split out so tests can spell the expected path without a second hand-join.
pub(crate) fn estate_home_graph_db_at(estate_root: &Path, repo: &Path) -> PathBuf {
    estate_root
        .join(repo_graph_key(repo))
        .join(CODE_GRAPH_DB_FILE)
}

/// Where `repo`'s code graph lives — or would live, for a repo never indexed. THE resolver: every
/// spelling of a per-repo graph path (the record's `code_graph_db`, the indexer's `--db`, the
/// dispatch-time MCP scope) comes from here.
///
/// LEGACY-FIRST: an existing `<repo>/.codegraph/estate.db` wins unconditionally (module ADR — an
/// indexed repo never migrates silently and never orphans). Only a repo with no in-tree graph
/// resolves to the estate home.
pub(crate) fn resolved_code_graph_db(repo: &Path) -> PathBuf {
    resolved_code_graph_db_at(repo, repo_graph_root().as_deref())
}

/// [`resolved_code_graph_db`] with the estate home injected — the pure core tests drive without
/// mutating process env.
fn resolved_code_graph_db_at(repo: &Path, estate_root: Option<&Path>) -> PathBuf {
    let legacy = repo.join(code_graph_rel());
    if legacy.is_file() {
        return legacy;
    }
    match estate_root {
        Some(root) => estate_home_graph_db_at(root, repo),
        None => legacy,
    }
}

/// Which home a RESOLVED `code_graph_db` value belongs to — the ONE shape recognition the sandbox
/// grants key off (`execute_wrapped::repo_read_root` / `graph_write_dir`). `None` means "not a
/// code graph": relative paths, wrong filenames, and paths under neither home are all fail-closed
/// (no grant), because taking a parent off an arbitrary path hands a worker an over-broad root.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CodeGraphHome {
    /// `<repo>/.codegraph/estate.db` — legacy, in-tree. Grant: READ on the repo root (the graph is
    /// inside it, and its file paths anchor there). Unchanged pre-ADR behavior.
    InTree { repo_root: PathBuf },
    /// `<estate_root>/<key>/estate.db` — the estate home. Grant: read+write on EXACTLY the key
    /// directory (WAL/journal siblings), NEVER the root above it — a worker must not be able to
    /// reach another repo's graph one sibling over.
    EstateHome { key_dir: PathBuf },
}

/// Classify an ABSOLUTE graph path against an injected estate root (pure; see the enum docs).
/// Callers resolving against the live environment pass `repo_graph_root().as_deref()`.
///
/// The estate-home arm matches only a db whose parent-of-parent IS `estate_root` and whose key
/// segment passes estate's label rule — an env root that moved since the path was minted, or a
/// key-shaped segment somewhere else on disk, classifies as nothing and grants nothing.
pub(crate) fn classify_code_graph_db_at(
    db: &Path,
    estate_root: Option<&Path>,
) -> Option<CodeGraphHome> {
    if !db.is_absolute() || db.file_name().is_none_or(|n| n != CODE_GRAPH_DB_FILE) {
        return None;
    }
    let dir = db.parent()?;
    if dir.file_name().is_some_and(|n| n == ".codegraph") {
        return dir.parent().map(|repo_root| CodeGraphHome::InTree {
            repo_root: repo_root.to_path_buf(),
        });
    }
    let root = estate_root?;
    if dir.parent() == Some(root) && dir.file_name().is_some_and(is_valid_key) {
        return Some(CodeGraphHome::EstateHome {
            key_dir: dir.to_path_buf(),
        });
    }
    None
}

/// Estate's label rule (`repo_scope.rs::validate_label`), applied to a key path segment: 1–64
/// chars of `[A-Za-z0-9._-]`, never `.`/`..`, no leading `-`. [`repo_graph_key`] mints only
/// passing keys; the classifier re-checks so a hand-built path cannot smuggle a traversal segment.
fn is_valid_key(seg: &std::ffi::OsStr) -> bool {
    let Some(s) = seg.to_str() else { return false };
    !s.is_empty()
        && s.len() <= 64
        && s != "."
        && s != ".."
        && !s.starts_with('-')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Serializes tests that MUTATE `WICKED_ESTATE_REPO_GRAPH_ROOT` (write side) against tests that
/// resolve through it (read side) — the acp_runner ENV_LOCK pattern (core#285), shared crate-wide
/// because the mutating tests live in more than one module.
#[cfg(test)]
pub(crate) static REPO_GRAPH_ROOT_ENV_LOCK: std::sync::RwLock<()> = std::sync::RwLock::new(());

/// [`CODE_GRAPH_DB_REL`] as a path, one segment at a time, so the separator is the platform's.
///
/// `repo.join(CODE_GRAPH_DB_REL)` looks like it does this and does not: `join` appends the argument
/// as a SINGLE component and leaves its `/` untouched, so on Windows it yields
/// `C:\repo\.codegraph/estate.db` — mixed separators, unequal to the `C:\repo\.codegraph\estate.db`
/// that crew's Node-side `join` produces for the same repo. Both open the same file, and every
/// comparison between them is false. The first cut of the FINDING-069 fix had exactly this bug,
/// with a doc comment asserting the opposite; Windows CI caught it and macOS/Linux could not have.
pub(crate) fn code_graph_rel() -> PathBuf {
    CODE_GRAPH_DB_REL.split('/').collect()
}

/// A repo's code-graph path, resolved for a WRITER, with its parent directory created.
///
/// Separate from [`existing_code_graph`] on purpose. This one is allowed to bring the file into
/// existence; the read side is not. Collapsing them is what made FINDING-069 undetectable: the
/// consumer called this, `create_dir_all` succeeded, and it returned a path to a database that had
/// never been indexed — so "no graph" and "graph right here" were the same value.
///
/// LEGACY-FIRST like every resolver arm: an already-indexed repo's writes keep landing on its
/// in-tree graph (refreshing the store crew's onboarding built, never forking a second one in the
/// estate home); only a repo with no in-tree graph mints there.
pub(crate) fn code_graph_path_for_write(repo: &Path) -> std::io::Result<PathBuf> {
    code_graph_path_for_write_at(repo, repo_graph_root().as_deref())
}

/// [`code_graph_path_for_write`] with the estate home injected (pure apart from the
/// `create_dir_all`; tests drive it at a scratch root so nothing touches a real home).
fn code_graph_path_for_write_at(
    repo: &Path,
    estate_root: Option<&Path>,
) -> std::io::Result<PathBuf> {
    let graph = resolved_code_graph_db_at(repo, estate_root);
    if let Some(parent) = graph.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(graph)
}

/// A repo's code graph if it has actually been indexed — `None` when the file is in NEITHER home
/// (legacy in-tree first, then the estate home; module ADR).
///
/// Creates nothing. A consumer choosing a store to hand a governed worker must treat `None` as "no
/// graph, ship no estate MCP" and never as license to substitute the operational store, which is the
/// store a worker can delete (FINDING-067).
pub(crate) fn existing_code_graph(repo: &Path) -> Option<PathBuf> {
    existing_code_graph_at(repo, repo_graph_root().as_deref())
}

/// [`existing_code_graph`] with the estate home injected — the pure core tests drive.
fn existing_code_graph_at(repo: &Path, estate_root: Option<&Path>) -> Option<PathBuf> {
    let graph = resolved_code_graph_db_at(repo, estate_root);
    graph.is_file().then_some(graph)
}

/// Index `repo` into its code graph via the wicked-estate indexer subprocess. Returns the db path.
pub fn index_repo(repo: &Path) -> anyhow::Result<String> {
    let graph = code_graph_path_for_write(repo)?;
    let graph_str = graph.to_string_lossy().to_string();
    let bin = indexer_bin();
    let out = Command::new(&bin)
        .hardened()
        .arg("index")
        .arg(repo)
        .arg("--db")
        .arg(&graph_str)
        .output()
        .map_err(|e| {
            anyhow::anyhow!(
                "could not run the `{bin}` indexer ({e}); install it (cargo install wicked-estate) \
                 or set WICKED_ESTATE_BIN to enable code-graph recon/ranking"
            )
        })?;
    if !out.status.success() {
        anyhow::bail!(
            "code indexer failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(graph_str)
}

/// Rank the top-`n` most central code symbols in an indexed graph (global PageRank over CALLS/IMPORTS).
pub fn rank_symbols(graph_path: &str, n: usize) -> anyhow::Result<Vec<RankedSymbol>> {
    let store = open_store(Some(graph_path))
        .map_err(|e| anyhow::anyhow!("open code graph {graph_path}: {e}"))?;
    // Total distinct source files — the denominator for the call-spread ubiquity test.
    let total_files = store
        .all_nodes()
        .map(|ns| {
            ns.iter()
                .map(|n| n.location.file.clone())
                .filter(|f| !f.is_empty())
                .collect::<HashSet<_>>()
                .len()
        })
        .unwrap_or(0)
        .max(1);
    // Scale-adaptive ubiquity cutoff: a symbol referenced from this FRACTION of files (or more) is a
    // generic/library utility. The fraction shrinks as the repo grows (in a 3000-file repo even a
    // ubiquitous symbol touches a smaller % of files than in a 20-file one) — fit 0.45/ln(files).
    let ubiq_frac = 0.45 / (total_files as f32).max(3.0).ln();
    // Over-fetch generously (filters are aggressive), then keep only MEANINGFUL source definitions.
    let ranked = wicked_estate_rank::ranked_symbols(&store, &[], n.saturating_mul(12).max(96))
        .map_err(|e| anyhow::anyhow!("rank code graph: {e}"))?;
    let mut out: Vec<(RankedSymbol, f32)> = ranked
        .into_iter()
        .filter_map(|(id, score)| {
            let node = store.get_node(&id).ok().flatten()?;
            // (1) definition kinds only; (2) from the original source (not tests/generated/vendor).
            if !is_def_kind(&node.kind) || is_excluded_path(&node.location.file) {
                return None;
            }
            // (3) ubiquity: referenced from ≥ the adaptive fraction of files (≥4 absolute) ⇒ generic
            // built-in / common-lib, not a domain hotspot.
            let spread = caller_spread(&store, &id);
            if total_files >= 6 && spread >= 4 && (spread as f32 / total_files as f32) >= ubiq_frac
            {
                return None;
            }
            Some((
                RankedSymbol {
                    name: node.name,
                    file: node.location.file,
                    kind: kind_str(&node.kind),
                    score_pct: 0,
                },
                score,
            ))
        })
        .collect();
    let top = out.first().map(|(_, s)| *s).unwrap_or(1.0).max(1e-9);
    for (sym, score) in &mut out {
        sym.score_pct = ((*score / top) * 100.0).round().clamp(0.0, 100.0) as u8;
    }
    out.truncate(n);
    Ok(out.into_iter().map(|(s, _)| s).collect())
}

/// Recon a repo end-to-end: index it, then return its `n` most central symbols. This is the
/// graph-native recon view — "what matters in this codebase" — fed to the CLIs + shown in the UI.
pub fn recon_repo(repo: &Path, n: usize) -> anyhow::Result<Vec<RankedSymbol>> {
    let graph = index_repo(repo)?;
    rank_symbols(&graph, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh scratch dir, keyed per test name + pid + thread (never reused across the pooled
    /// test threads).
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wicked-cg-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The consumer-facing literals, pinned. Every out-of-process consumer joins the LEGACY
    /// spelling onto a repo root — crew did it in five places, and when the engine's spelling and
    /// crew's disagreed the worker got a database nothing had written (FINDING-069). Changing
    /// either is a coordinated release, not a rename.
    #[test]
    fn the_spellings_are_the_ones_consumers_expect() {
        assert_eq!(CODE_GRAPH_DB_REL, ".codegraph/estate.db");
        assert_eq!(CODE_GRAPH_DB_FILE, "estate.db");
        assert_eq!(REPO_GRAPH_ROOT_ENV, "WICKED_ESTATE_REPO_GRAPH_ROOT");
        // Joined SEGMENT BY SEGMENT, so the separator is the platform's and a consumer's
        // `join(root, '.codegraph', 'estate.db')` produces a byte-identical string. This is a
        // no-op on Unix and load-bearing on Windows: `Path::join` given the whole
        // `.codegraph/estate.db` appends it as one component and leaves the `/` alone, which on
        // Unix is indistinguishable from doing it right (the first cut of the FINDING-069 fix did
        // exactly that and only Windows CI could see it). Do not "simplify" the two `join`s back
        // into one.
        assert_eq!(
            Path::new("/repo").join(code_graph_rel()),
            Path::new("/repo").join(".codegraph").join("estate.db"),
        );
    }

    /// The override wins over the home; the home default is `<home>/.wicked-estate/repo-graphs`;
    /// no home at all resolves to nothing (the resolver then stays in-tree). Pure — no env.
    #[test]
    fn the_root_is_the_override_then_the_home_then_nothing() {
        let over = Some(std::ffi::OsString::from("/x/graphs"));
        let home = Some(std::ffi::OsString::from("/home/u"));
        assert_eq!(
            repo_graph_root_from(over.clone(), home.clone()),
            Some(PathBuf::from("/x/graphs")),
            "the env override wins outright"
        );
        assert_eq!(
            repo_graph_root_from(None, home),
            Some(
                Path::new("/home/u")
                    .join(".wicked-estate")
                    .join("repo-graphs")
            ),
            "no override falls back to the home default"
        );
        assert_eq!(
            repo_graph_root_from(Some(std::ffi::OsString::new()), None),
            None,
            "an EMPTY override does not name a root, and no home resolves to nothing"
        );
    }

    /// The key: `<dir-name>-<12 hex>`, estate-label-legal, stable across spellings of one root,
    /// distinct across different roots sharing a dir name.
    #[test]
    fn the_key_is_the_dir_name_plus_a_digest_and_is_label_legal() {
        let a = scratch("key-a").join("api");
        let b = scratch("key-b").join("api");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();

        let key_a = repo_graph_key(&a);
        let key_b = repo_graph_key(&b);
        assert!(
            key_a.starts_with("api-"),
            "the dir name leads, so an operator can map the key back to a repo: {key_a}"
        );
        let hex = &key_a[key_a.rfind('-').unwrap() + 1..];
        assert_eq!(hex.len(), KEY_HASH_LEN, "12-hex digest tail: {key_a}");
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()), "{key_a}");
        assert_ne!(
            key_a, key_b,
            "two repos sharing a dir name must get distinct keys — the digest is over the root"
        );
        assert_eq!(
            repo_graph_key(&a),
            key_a,
            "same root ⇒ same key, every time"
        );
        // Canonicalization folds spellings: `<a>/.` and `<a>` are the same repo.
        assert_eq!(repo_graph_key(&a.join(".")), key_a);

        // Every minted key passes estate's label rule — it becomes a path segment.
        for key in [&key_a, &key_b] {
            assert!(
                is_valid_key(std::ffi::OsStr::new(key)),
                "a minted key must pass the label rule it is checked against: {key}"
            );
            assert!(key.len() <= 64, "estate's 64-byte label ceiling: {key}");
        }

        // Sanitization: illegal chars collapse to `-`, and the key still carries the digest.
        let weird = scratch("key-w").join("a b@c");
        std::fs::create_dir_all(&weird).unwrap();
        let key_w = repo_graph_key(&weird);
        assert!(key_w.starts_with("a-b-c-"), "{key_w}");
        assert!(is_valid_key(std::ffi::OsStr::new(&key_w)), "{key_w}");
    }

    /// AC2 — CONTINUITY, the hard requirement: a repo with an in-tree graph keeps it for read AND
    /// write, even when a perfectly good estate home (holding this very repo's key!) exists. An
    /// already-indexed repo never migrates silently and never orphans (FINDING-069's lesson).
    #[test]
    fn a_repo_with_a_legacy_in_tree_graph_keeps_it_for_read_and_write() {
        let base = scratch("legacy-first");
        let repo = base.join("repo");
        let legacy = repo.join(code_graph_rel());
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, b"185 MB of graph, notionally").unwrap();
        // A rival estate-home graph for the SAME repo — the resolver must not even look at it.
        let estate_root = base.join("estate");
        let rival = estate_home_graph_db_at(&estate_root, &repo);
        std::fs::create_dir_all(rival.parent().unwrap()).unwrap();
        std::fs::write(&rival, b"an empty fork nothing should ever read").unwrap();

        assert_eq!(
            resolved_code_graph_db_at(&repo, Some(&estate_root)),
            legacy,
            "resolution is legacy-first"
        );
        assert_eq!(
            existing_code_graph_at(&repo, Some(&estate_root)).as_deref(),
            Some(legacy.as_path()),
            "the READ path keeps the in-tree graph"
        );
        assert_eq!(
            code_graph_path_for_write_at(&repo, Some(&estate_root)).unwrap(),
            legacy,
            "the WRITE path keeps refreshing the in-tree graph — never a silent fork"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// AC1's resolver half: a repo with NO in-tree graph mints in the estate home — the key dir is
    /// created under the injected root, the working tree stays untouched, and the read side still
    /// answers `None` until something actually indexes.
    #[test]
    fn a_repo_without_a_legacy_graph_mints_in_the_estate_home() {
        let base = scratch("estate-mint");
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let estate_root = base.join("estate");

        let want = estate_home_graph_db_at(&estate_root, &repo);
        assert_eq!(resolved_code_graph_db_at(&repo, Some(&estate_root)), want);
        assert_eq!(
            existing_code_graph_at(&repo, Some(&estate_root)),
            None,
            "no graph anywhere ⇒ None — never a path to a database nothing wrote (FINDING-069)"
        );

        let for_write = code_graph_path_for_write_at(&repo, Some(&estate_root)).unwrap();
        assert_eq!(for_write, want);
        assert!(want.parent().unwrap().is_dir(), "the key dir is created");
        assert!(
            !repo.join(code_graph_rel()).parent().unwrap().exists(),
            "the working tree is NOT polluted — that is the whole point of the estate home"
        );

        // Once the indexer writes the file, the read side finds it there.
        std::fs::write(&want, b"indexed").unwrap();
        assert_eq!(
            existing_code_graph_at(&repo, Some(&estate_root)).as_deref(),
            Some(want.as_path())
        );

        // No home at all: the resolver stays in-tree (the only address left).
        assert_eq!(
            resolved_code_graph_db_at(&repo, None),
            repo.join(code_graph_rel())
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// AC3 — the env override, END TO END through the public (env-reading) resolver: everything
    /// lands under the override, and the real default home's key dir for this repo is never
    /// created. Holds the crate-wide write lock; every other resolver test injects its root and
    /// never reads env.
    #[test]
    fn the_env_override_redirects_the_root_away_from_the_real_home() {
        let _env = REPO_GRAPH_ROOT_ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var_os(REPO_GRAPH_ROOT_ENV);

        let base = scratch("env-override");
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let estate_root = base.join("estate");
        std::env::set_var(REPO_GRAPH_ROOT_ENV, &estate_root);

        let resolved = resolved_code_graph_db(&repo);
        let for_write = code_graph_path_for_write(&repo).unwrap();
        assert_eq!(resolved, estate_home_graph_db_at(&estate_root, &repo));
        assert_eq!(for_write, resolved);
        assert!(resolved.starts_with(&estate_root), "{resolved:?}");

        // Nothing under the DEFAULT home root for this repo's key — the override redirected it.
        if let Some(default_root) = repo_graph_root_from(
            None,
            std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")),
        ) {
            assert!(
                !default_root.join(repo_graph_key(&repo)).exists(),
                "the override must keep the real home untouched"
            );
        }

        match prev {
            Some(v) => std::env::set_var(REPO_GRAPH_ROOT_ENV, v),
            None => std::env::remove_var(REPO_GRAPH_ROOT_ENV),
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The shape recognition the sandbox grants key off: exactly the two homes, nothing else.
    #[test]
    fn classification_recognizes_exactly_the_two_shapes() {
        let base = scratch("classify");
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let estate_root = base.join("estate");
        let key = repo_graph_key(&repo);

        // Legacy in-tree → InTree, repo root recovered.
        assert_eq!(
            classify_code_graph_db_at(&repo.join(code_graph_rel()), Some(&estate_root)),
            Some(CodeGraphHome::InTree {
                repo_root: repo.clone()
            }),
        );
        // Estate home → EstateHome, EXACTLY the key dir.
        let db = estate_home_graph_db_at(&estate_root, &repo);
        assert_eq!(
            classify_code_graph_db_at(&db, Some(&estate_root)),
            Some(CodeGraphHome::EstateHome {
                key_dir: estate_root.join(&key)
            }),
        );

        // Everything else is NOT a graph (fail-closed: no grant).
        for (why, bad) in [
            ("relative", PathBuf::from("repo").join(code_graph_rel())),
            ("wrong filename", estate_root.join(&key).join("other.db")),
            (
                "db directly under the root",
                estate_root.join(CODE_GRAPH_DB_FILE),
            ),
            (
                "key dir under a DIFFERENT root",
                base.join("elsewhere").join(&key).join(CODE_GRAPH_DB_FILE),
            ),
            (
                "traversal-shaped key segment",
                estate_root.join("..").join(CODE_GRAPH_DB_FILE),
            ),
        ] {
            assert!(
                classify_code_graph_db_at(&bad, Some(&estate_root)).is_none(),
                "{why} must not classify as a graph: {bad:?}"
            );
        }
        // And with NO estate root resolvable, only the legacy shape classifies.
        assert!(classify_code_graph_db_at(&db, None).is_none());

        let _ = std::fs::remove_dir_all(&base);
    }
}
