//! CODE GRAPH — operate GRAPH-NATIVELY on a repo's code/domain graph, the wicked-estate substrate the
//! whole methodology spine (recon → review → test), memory cross-edges, and routing are built on.
//!
//! ARCHITECTURE: indexing (the heavy 150+ tree-sitter language extractors) is delegated to the
//! `wicked-estate` indexer as a SUBPROCESS, so the grammars stay OUT of this engine/UI binary. The
//! engine then READS + RANKS the resulting graph with the lean `estate-core` (GraphRead) +
//! `estate-rank` (PageRank) crates it already links. Indexing is a build step; operating on the graph
//! is the runtime — keeping them separate is what lets us be graph-native without bloat.
//!
//! # WHERE A REPO'S GRAPH LIVES (ADR, 2026-08-29; revised 2026-09-10 for core#406)
//!
//! **Under the daemon's state home.** A repo's graph is at `<root>/<key>/estate.db`, where `root`
//! is [`repo_graph_root`] — in precedence order:
//!
//! 1. `$WICKED_ESTATE_REPO_GRAPH_ROOT`, when set (the escape hatch for tests and proof scripts —
//!    the same contract as crew's `WICKED_CREW_PROJECT_GRAPH_ROOT`; an ABSOLUTE path, the sandbox
//!    grants fail closed on relative ones);
//! 2. `<state home>/repo-graphs`, where the state home is the canonical parent of the engine's
//!    own `--db` (`state_home::operational_home_of_db`) — the ONE storage root the operator keeps,
//!    the directory crew's `--db` relocates wholesale (crew#330), and the directory the worker Read
//!    fence classifies entry by entry (`repo-graphs` is registered in
//!    `tests/fixtures/state-home-subtrees.json`, byte-identical in core and crew);
//! 3. `<home>/.wicked-crew/repo-graphs` — the DEFAULT state home — for a library consumer or test
//!    thread that never spawned a `Core` (the fallback crew's `crewStateHome()` makes for the
//!    same callers);
//!
//! and `<key>` is [`repo_graph_key`]'s `<repo-dir-name>-<12-hex-of-sha256(canonical-root)>`. A
//! directory per key (rather than `<key>.db` files in one flat folder) keeps the db and its
//! WAL/journal siblings together, so removing a repo's graph is one `rm -rf` that cannot strand a
//! `-wal` describing a database that is gone.
//!
//! The state home reaches this resolver through [`StateHomeScope`]: the actor binds its thread to
//! the store it was spawned on before it serves a command (the `GOV_DB_PATH` idiom), and the two
//! off-actor readers (`repo::coverage_report_for_repo`, `repo::graph_kinds_for_repo`) bind a
//! scope from the store path they were handed. The launchers, which already carry the runner's
//! `operational_home`, pass it explicitly ([`repo_graph_root_for`]). No process-global: two
//! engines in one process (crew's test workers) resolve independently.
//!
//! **Why not the estate home.** The 2026-08 cut minted graphs under `<home>/.wicked-estate/
//! repo-graphs` — the OPERATOR's home, whatever `--db` said. Two daemons on one host shared and
//! clobbered each other's graphs, `--db` did not relocate the data a customer backs up or isolates,
//! and crew's diagnostics could not list a store outside the state home (core#406, F-016). That
//! directory is now read exactly once, at boot, as the MIGRATION SOURCE — see below — and never
//! written.
//!
//! **Never inside the working tree.** The pre-ADR default — `<repo>/.codegraph/estate.db`,
//! INSIDE the checkout — polluted every tree it touched, and the 2026-08 "legacy-first" rule that
//! kept adopting an existing in-tree file made placement NON-DETERMINISTIC across repos: a checkout
//! whose git history happened to TRACK `.codegraph/estate.db` (two of the family's own repos did)
//! got its graph written INTO the customer's tree by "read-only" onboarding — `git status` dirty,
//! a `git checkout .` silently reverting the graph (core#406, F-024). So this resolver NEVER reads
//! or writes a graph inside `root_path`: an in-tree `.codegraph/` is IGNORED and REPORTED — a
//! `findings` entry on the repo record (`repo::RepoFinding`, code `in_tree_code_graph_ignored`)
//! names the directory so the operator can delete/untrack it — and the live graph is minted under
//! the state home like every other repo's. The in-tree spelling survives here only as
//! [`CODE_GRAPH_DB_REL`] / [`in_tree_code_graph_dir`], to recognise and report it.
//!
//! **Migration, once, at boot.** The first actor boot over a store whose registered repos have a
//! graph under the old `<home>/.wicked-estate/repo-graphs/<key>` and none under the new root copies
//! each one — [`migrate_legacy_repo_graphs`], through SQLite's online-backup API (page-consistent
//! even for a WAL-mode db another connection still holds open; a plain file copy of
//! `estate.db`+`-wal`+`-shm` is not) — and logs one line per repo. The key is a pure function of
//! the repo root, so the source and destination agree without a lookup table. The SOURCE IS LEFT
//! IN PLACE (an operator deletes `~/.wicked-estate/repo-graphs` once the new daemon is verified;
//! the engine never deletes anything it did not write), a destination that already exists is
//! never overwritten, and a copy that fails is removed so the repo simply re-indexes at its next
//! onboarding instead of reading a torn database.
//!
//! **Per-key sandbox grants.** A governed worker is granted read+write on EXACTLY its own
//! `<root>/<key>/` directory (write because opening a WAL-mode SQLite db creates `-wal`/`-shm`/
//! journal files in its directory) — never the `repo-graphs` root or the state home, because every
//! OTHER repo's graph lives one sibling over and a worker must not be able to reach it.
//! [`classify_code_graph_db_at`] is the one shape recogniser both grants key off; a path in the
//! in-tree shape classifies as NOTHING (no grant — the graph is never there). Note the trade: a
//! graph's file paths still anchor to the repo root the indexer ran over, but the per-key grant
//! does not include that root — a worker reads source from its own worktree instead.

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

/// The LEGACY in-tree spelling, `<repo>/.codegraph/estate.db` — a TEST STAND-IN now.
///
/// `.codegraph/estate.db` is the path crew's pre-ADR onboarding indexed to (`wicked-estate index
/// --db <repo>/.codegraph/estate.db`), and for a while the engine's own spelling disagreed with it
/// (`.wicked/code-graph.db`), so a worker queried a database nothing had written (FINDING-069).
/// The ONE spelling was then pinned here and the resolver kept ADOPTING an existing in-tree file.
/// core#406 ends that: no production code path spells this file any more (the module ADR) —
/// production recognises the DIRECTORY, [`IN_TREE_CODE_GRAPH_DIR`], to report it. The tests keep
/// this constant to build the decoys and stand-in paths they prove are never adopted or widened.
///
/// Written with `/` because that is how every other artifact in the ecosystem spells it. Do NOT
/// hand it to [`Path::join`] whole; use [`code_graph_rel`], which is the only correct way to turn
/// it into a path.
#[cfg(test)]
pub(crate) const CODE_GRAPH_DB_REL: &str = ".codegraph/estate.db";

/// `<repo>/.codegraph` — the directory a pre-core#406 engine (or crew's pre-ADR onboarding) indexed
/// INTO the working tree. The ONE production spelling of the in-tree shape: never resolved,
/// recognised by [`has_in_tree_code_graph`] so the repo record can report it.
pub(crate) const IN_TREE_CODE_GRAPH_DIR: &str = ".codegraph";

/// The filename every code-graph database carries: `<root>/<key>/estate.db`.
pub(crate) const CODE_GRAPH_DB_FILE: &str = "estate.db";

/// Env var overriding the repo-graph root wholesale — precedence 1 in [`repo_graph_root`] and the
/// module ADR. TH-8's environment manifest should list it.
pub(crate) const REPO_GRAPH_ROOT_ENV: &str = "WICKED_ESTATE_REPO_GRAPH_ROOT";

/// The state-home subtree every repo graph hangs off: `<state home>/repo-graphs`. Registered in
/// `tests/fixtures/state-home-subtrees.json` (owner `engine`; crew mirrors the file byte for byte)
/// so the worker Read fence classifies — and denies — it like every other state-home store.
pub(crate) const REPO_GRAPHS_DIRNAME: &str = "repo-graphs";

/// Where the pre-core#406 engine kept every repo graph: `<home>/.wicked-estate/repo-graphs`. Read
/// once, at boot, as the migration SOURCE ([`migrate_legacy_repo_graphs`]); never written.
pub(crate) const LEGACY_ESTATE_HOME_DIRNAME: &str = ".wicked-estate";

/// `$HOME`, or `$USERPROFILE` on Windows.
fn home_dir() -> Option<std::ffi::OsString> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
}

thread_local! {
    /// The state home this thread's resolver is bound to — the actor thread binds the store it was
    /// spawned on ([`StateHomeScope::for_store`]); an unbound thread resolves the default state
    /// home. Thread-local rather than process-global on purpose: two engines in one process (crew's
    /// test workers, a harness spawning several `Core`s) must resolve independently, and a
    /// process-global set by the latest spawn would silently hand one daemon's graphs to another's
    /// state home.
    static BOUND_STATE_HOME: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// RAII binding of the calling thread's repo-graph resolver to a state home (module ADR). Holds
/// for the guard's lifetime and restores the previous binding on drop, so an off-actor reader can
/// scope a store's state home around one `get_repo` without disturbing the thread it runs on.
#[must_use = "the binding lasts only as long as this guard is held"]
pub(crate) struct StateHomeScope {
    prev: Option<PathBuf>,
}

impl StateHomeScope {
    /// Bind to the state home of the store at `db_path` — its canonical parent directory
    /// (`state_home::operational_home_of_db`); `:memory:` and `postgres://` stores have none, and
    /// the thread then resolves the default state home.
    pub(crate) fn for_store(db_path: &str) -> Self {
        Self::bind(crate::state_home::operational_home_of_db(db_path))
    }

    /// Bind to an explicit state home (`None` unbinds).
    pub(crate) fn bind(state_home: Option<PathBuf>) -> Self {
        let prev = BOUND_STATE_HOME.with(|c| c.replace(state_home));
        StateHomeScope { prev }
    }
}

impl Drop for StateHomeScope {
    fn drop(&mut self) {
        let prev = self.prev.take();
        BOUND_STATE_HOME.with(|c| *c.borrow_mut() = prev);
    }
}

/// The state home the calling thread is bound to, if any.
pub(crate) fn bound_state_home() -> Option<PathBuf> {
    BOUND_STATE_HOME.with(|c| c.borrow().clone())
}

/// The root every repo graph hangs off, for the CALLING THREAD: the env override, else the bound
/// state home's `repo-graphs`, else the default state home's (module ADR, precedence 1–3). `None`
/// only when no override is set and no home can be resolved at all; the resolvers then answer
/// nothing — never an in-tree path.
pub(crate) fn repo_graph_root() -> Option<PathBuf> {
    repo_graph_root_for(bound_state_home().as_deref())
}

/// [`repo_graph_root`] for an EXPLICIT state home — what the launchers pass (they already carry
/// the runner's `operational_home`, derived from the same `--db` the actor binds). Env override
/// first, then `state_home`, then the default state home.
pub(crate) fn repo_graph_root_for(state_home: Option<&Path>) -> Option<PathBuf> {
    repo_graph_root_from(
        std::env::var_os(REPO_GRAPH_ROOT_ENV),
        state_home,
        home_dir(),
    )
}

/// The repo-graph root of the daemon whose operational store is at `db_path`:
/// `<canonical parent of db_path>/repo-graphs` (env override first, default state home when the
/// path names no directory — `:memory:`, `postgres://`). The one spelling an out-of-process
/// consumer (crew's diagnostics, a proof script) needs to locate a daemon's repo graphs.
pub fn repo_graph_root_for_store(db_path: &str) -> Option<PathBuf> {
    repo_graph_root_for(crate::state_home::operational_home_of_db(db_path).as_deref())
}

/// [`repo_graph_root`]'s pure core, split out so the precedence is testable without mutating
/// process env (env mutation races parallel tests; the few tests that must mutate hold
/// [`REPO_GRAPH_ROOT_ENV_LOCK`]).
fn repo_graph_root_from(
    override_root: Option<std::ffi::OsString>,
    state_home: Option<&Path>,
    home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    if let Some(r) = override_root {
        if !r.is_empty() {
            return Some(PathBuf::from(r));
        }
    }
    if let Some(sh) = state_home {
        return Some(sh.join(REPO_GRAPHS_DIRNAME));
    }
    home.filter(|h| !h.is_empty()).map(|h| {
        Path::new(&h)
            .join(crate::state_home::DEFAULT_STATE_HOME_DIRNAME)
            .join(REPO_GRAPHS_DIRNAME)
    })
}

/// The pre-core#406 root, `<home>/.wicked-estate/repo-graphs` — the migration SOURCE only.
pub(crate) fn legacy_repo_graph_root() -> Option<PathBuf> {
    legacy_repo_graph_root_from(home_dir())
}

fn legacy_repo_graph_root_from(home: Option<std::ffi::OsString>) -> Option<PathBuf> {
    home.filter(|h| !h.is_empty()).map(|h| {
        Path::new(&h)
            .join(LEGACY_ESTATE_HOME_DIRNAME)
            .join(REPO_GRAPHS_DIRNAME)
    })
}

/// Sanitized-stem budget: 51 + `-` + 12 hex = exactly estate's 64-byte label ceiling — the same
/// arithmetic as crew's `graph-paths.ts` (its `HASH_LEN`/`STEM_LEN`).
const KEY_HASH_LEN: usize = 12;
const KEY_STEM_LEN: usize = 64 - 1 - KEY_HASH_LEN;

/// The graph directory key for one repo:
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
/// indexer, the dispatch-time resolver, and the boot-time migration all land on the same
/// directory, under whichever root is live.
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

/// One repo's graph db under a given root — `<root>/<key>/estate.db` — split out so tests can
/// spell the expected path without a second hand-join.
pub(crate) fn repo_graph_db_at(root: &Path, repo: &Path) -> PathBuf {
    root.join(repo_graph_key(repo)).join(CODE_GRAPH_DB_FILE)
}

/// Where `repo`'s code graph lives — or would live, for a repo never indexed. THE resolver: every
/// spelling of a per-repo graph path (the record's `code_graph_db`, the indexer's `--db`, the
/// dispatch-time MCP scope) comes from here. `None` when no root resolves at all (module ADR) —
/// never a path inside `repo`.
pub(crate) fn resolved_code_graph_db(repo: &Path) -> Option<PathBuf> {
    resolved_code_graph_db_at(repo, repo_graph_root().as_deref())
}

/// [`resolved_code_graph_db`] with the root injected — the pure core tests drive without
/// mutating process env. Whether `<repo>/.codegraph/` exists is deliberately NOT consulted.
fn resolved_code_graph_db_at(repo: &Path, root: Option<&Path>) -> Option<PathBuf> {
    root.map(|r| repo_graph_db_at(r, repo))
}

/// `<repo>/.codegraph` — the directory a pre-core#406 engine (or crew's pre-ADR onboarding)
/// indexed INTO the working tree. Never resolved; recognised so it can be reported.
pub(crate) fn in_tree_code_graph_dir(repo: &Path) -> PathBuf {
    repo.join(IN_TREE_CODE_GRAPH_DIR)
}

/// Whether the checkout at `repo` carries a `.codegraph` entry of ANY kind (directory, stray
/// file, symlink — `symlink_metadata`, so a dangling link is still reported and never followed).
/// The repo record turns a `true` into its `in_tree_code_graph_ignored` finding.
pub(crate) fn has_in_tree_code_graph(repo: &Path) -> bool {
    std::fs::symlink_metadata(in_tree_code_graph_dir(repo)).is_ok()
}

/// Classify an ABSOLUTE graph path against an injected root (pure): `Some(<root>/<key>)` — the
/// EXACT key directory — for `<root>/<key>/estate.db` whose key segment passes estate's label
/// rule, `None` for everything else. This is the ONE shape recognition the sandbox grants key off
/// (`execute_wrapped::repo_read_root` / `graph_write_dir`): relative paths, wrong filenames, a db
/// directly under the root, a key dir under a DIFFERENT root (an env root that moved since the
/// path was minted), and the legacy in-tree shape `<repo>/.codegraph/estate.db` all classify as
/// nothing and grant nothing — taking a parent off an arbitrary path hands a worker an over-broad
/// root, and a graph is never in the tree (module ADR).
pub(crate) fn classify_code_graph_db_at(db: &Path, root: Option<&Path>) -> Option<PathBuf> {
    if !db.is_absolute() || db.file_name().is_none_or(|n| n != CODE_GRAPH_DB_FILE) {
        return None;
    }
    let dir = db.parent()?;
    let root = root?;
    if dir.parent() == Some(root) && dir.file_name().is_some_and(is_valid_key) {
        return Some(dir.to_path_buf());
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
/// because the mutating tests live in more than one module. A bound [`StateHomeScope`] is
/// thread-local and needs no lock; the env override is process-wide and does.
#[cfg(test)]
pub(crate) static REPO_GRAPH_ROOT_ENV_LOCK: std::sync::RwLock<()> = std::sync::RwLock::new(());

/// [`CODE_GRAPH_DB_REL`] as a path, one segment at a time, so the separator is the platform's
/// (test stand-in, like the constant).
///
/// `repo.join(CODE_GRAPH_DB_REL)` looks like it does this and does not: `join` appends the argument
/// as a SINGLE component and leaves its `/` untouched, so on Windows it yields
/// `C:\repo\.codegraph/estate.db` — mixed separators, unequal to the `C:\repo\.codegraph\estate.db`
/// that crew's Node-side `join` produces for the same repo. Both open the same file, and every
/// comparison between them is false. The first cut of the FINDING-069 fix had exactly this bug,
/// with a doc comment asserting the opposite; Windows CI caught it and macOS/Linux could not have.
#[cfg(test)]
pub(crate) fn code_graph_rel() -> PathBuf {
    CODE_GRAPH_DB_REL.split('/').collect()
}

/// A repo's code-graph path, resolved for a WRITER, with its key directory created.
///
/// Separate from [`existing_code_graph`] on purpose. This one is allowed to bring the directory into
/// existence; the read side is not. Collapsing them is what made FINDING-069 undetectable: the
/// consumer called this, `create_dir_all` succeeded, and it returned a path to a database that had
/// never been indexed — so "no graph" and "graph right here" were the same value.
///
/// Always under the live root (module ADR): a repo carrying an in-tree `.codegraph/` still mints
/// here, never refreshes the in-tree file. An error when no root resolves — never a fallback into
/// the working tree.
pub(crate) fn code_graph_path_for_write(repo: &Path) -> std::io::Result<PathBuf> {
    code_graph_path_for_write_at(repo, repo_graph_root().as_deref())
}

/// [`code_graph_path_for_write`] with the root injected (pure apart from the `create_dir_all`;
/// tests drive it at a scratch root so nothing touches a real home).
fn code_graph_path_for_write_at(repo: &Path, root: Option<&Path>) -> std::io::Result<PathBuf> {
    let graph = resolved_code_graph_db_at(repo, root).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "no repo-graph root resolves for {}: set {REPO_GRAPH_ROOT_ENV}, run under a daemon \
                 (the root is <state home>/{REPO_GRAPHS_DIRNAME}), or set HOME — a graph is never \
                 minted inside the working tree",
                repo.display()
            ),
        )
    })?;
    if let Some(parent) = graph.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(graph)
}

/// A repo's code graph if it has actually been indexed — `None` when the file is not under the
/// live root (module ADR). An in-tree `<repo>/.codegraph/estate.db` is NOT a graph this engine
/// will read.
///
/// Creates nothing. A consumer choosing a store to hand a governed worker must treat `None` as "no
/// graph, ship no estate MCP" and never as license to substitute the operational store, which is the
/// store a worker can delete (FINDING-067).
pub(crate) fn existing_code_graph(repo: &Path) -> Option<PathBuf> {
    existing_code_graph_at(repo, repo_graph_root().as_deref())
}

/// [`existing_code_graph`] with the root injected — the pure core tests drive.
fn existing_code_graph_at(repo: &Path, root: Option<&Path>) -> Option<PathBuf> {
    resolved_code_graph_db_at(repo, root).filter(|g| g.is_file())
}

// ── one-time migration from the estate home (core#406) ──────────────────────────────────────────

/// What the boot-time migration did for one registered repo. Repos with nothing to migrate (no
/// legacy graph, or a graph already under the live root) produce no entry — the common case, and
/// silent on purpose: this runs on every boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GraphMigration {
    /// The legacy graph was copied to the live root. The source is left in place.
    Copied {
        repo_id: String,
        from: PathBuf,
        to: PathBuf,
    },
    /// The copy failed; the partial destination was removed, so the repo re-indexes at its next
    /// onboarding rather than reading a torn database. The source is untouched.
    Failed {
        repo_id: String,
        from: PathBuf,
        to: PathBuf,
        error: String,
    },
}

impl GraphMigration {
    /// The one-line operator notice the actor logs.
    pub(crate) fn notice(&self) -> String {
        match self {
            GraphMigration::Copied { repo_id, from, to } => format!(
                "wicked-core: migrated repo `{repo_id}`'s code graph {} -> {} (core#406: repo \
                 graphs live under the daemon state home); the old copy is left in place — remove \
                 the old `{LEGACY_ESTATE_HOME_DIRNAME}/{REPO_GRAPHS_DIRNAME}` directory once this \
                 daemon is verified",
                from.display(),
                to.display()
            ),
            GraphMigration::Failed {
                repo_id,
                from,
                to,
                error,
            } => format!(
                "wicked-core: could not migrate repo `{repo_id}`'s code graph {} -> {}: {error}; \
                 the old copy is untouched and the repo will re-index at its next onboarding",
                from.display(),
                to.display()
            ),
        }
    }
}

/// Bring every registered repo's graph from the pre-core#406 estate home under the live root —
/// ONCE (a destination that exists is never touched), through SQLite's online-backup API, leaving
/// the source in place (module ADR). `repos` is `(repo id, repo root)` for each registered repo;
/// the root of the calling thread ([`repo_graph_root`]) is the destination.
pub(crate) fn migrate_legacy_repo_graphs<'a>(
    repos: impl IntoIterator<Item = (&'a str, &'a Path)>,
) -> Vec<GraphMigration> {
    match (repo_graph_root(), legacy_repo_graph_root()) {
        (Some(root), Some(legacy)) => migrate_legacy_repo_graphs_at(repos, &root, &legacy),
        _ => Vec::new(),
    }
}

/// [`migrate_legacy_repo_graphs`] with both roots injected — pure apart from the copies.
fn migrate_legacy_repo_graphs_at<'a>(
    repos: impl IntoIterator<Item = (&'a str, &'a Path)>,
    root: &Path,
    legacy_root: &Path,
) -> Vec<GraphMigration> {
    // The override can name the legacy directory itself (an operator who pinned it); then there
    // is nothing to move and copying a graph onto itself would be the one way to corrupt it.
    if same_root(root, legacy_root) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (repo_id, repo) in repos {
        let from = repo_graph_db_at(legacy_root, repo);
        let to = repo_graph_db_at(root, repo);
        if to.exists() || !from.is_file() {
            continue;
        }
        out.push(match copy_sqlite_db(&from, &to) {
            Ok(()) => GraphMigration::Copied {
                repo_id: repo_id.to_string(),
                from,
                to,
            },
            Err(error) => {
                // Never leave a torn destination: `existing_code_graph` would hand it to a worker.
                if let Some(dir) = to.parent() {
                    let _ = std::fs::remove_dir_all(dir);
                }
                GraphMigration::Failed {
                    repo_id: repo_id.to_string(),
                    from,
                    to,
                    error,
                }
            }
        });
    }
    out
}

/// Whether two roots name the same directory, by canonical spelling when both exist and by
/// lexical equality otherwise.
fn same_root(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Copy one SQLite database `from` → `to` with the online-backup API: a page-consistent snapshot
/// even of a WAL-mode db another process still has open, which a byte copy of `estate.db` +
/// `-wal` + `-shm` is not. `to`'s directory is created; `to` must not exist. Bounded: a source
/// that stays locked (`Busy`/`Locked` for more than a few seconds) is an error, never a boot that
/// hangs.
fn copy_sqlite_db(from: &Path, to: &Path) -> Result<(), String> {
    use rusqlite::{backup::Backup, backup::StepResult, Connection, OpenFlags};
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    let src = Connection::open_with_flags(
        from,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("open source {}: {e}", from.display()))?;
    let mut dst =
        Connection::open(to).map_err(|e| format!("create destination {}: {e}", to.display()))?;
    let backup = Backup::new(&src, &mut dst).map_err(|e| format!("start backup: {e}"))?;
    // 256 pages per step; up to ~5 s of a locked source before giving up.
    const MAX_LOCKED_STEPS: u32 = 200;
    let mut locked_steps = 0u32;
    loop {
        match backup.step(256).map_err(|e| format!("backup step: {e}"))? {
            StepResult::Done => return Ok(()),
            StepResult::More => {}
            StepResult::Busy | StepResult::Locked => {
                locked_steps += 1;
                if locked_steps > MAX_LOCKED_STEPS {
                    return Err("source database stayed locked".to_string());
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            // `StepResult` is `#[non_exhaustive]`: a variant this rusqlite does not know is not a
            // copy we can vouch for — fail closed (the caller removes the torn destination).
            other => return Err(format!("unexpected backup step result: {other:?}")),
        }
    }
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

/// Test support shared by every module whose tests need a KNOWN repo-graph root.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};

    /// Pin the repo-graph root to `<dir>/repo-graphs` for the pin's lifetime: holds the crate-wide
    /// env WRITE lock, sets `WICKED_ESTATE_REPO_GRAPH_ROOT` (precedence 1 — it outranks any bound
    /// state home and the default home alike), and restores the previous value on drop. What a
    /// test gets: a deterministic root whatever thread binding or process env is live, and
    /// hermeticity — nothing resolves into, or is minted under, a real home. Do not take the env
    /// lock again while holding a pin (`RwLock` is not reentrant).
    #[must_use = "the pin lasts only as long as this guard is held"]
    pub(crate) struct GraphRootPin {
        _env: std::sync::RwLockWriteGuard<'static, ()>,
        prev: Option<std::ffi::OsString>,
        pub root: PathBuf,
    }

    impl GraphRootPin {
        pub(crate) fn at(dir: &Path) -> Self {
            let env = super::REPO_GRAPH_ROOT_ENV_LOCK
                .write()
                .unwrap_or_else(|p| p.into_inner());
            let prev = std::env::var_os(super::REPO_GRAPH_ROOT_ENV);
            let root = dir.join(super::REPO_GRAPHS_DIRNAME);
            std::fs::create_dir_all(&root).unwrap();
            std::env::set_var(super::REPO_GRAPH_ROOT_ENV, &root);
            GraphRootPin {
                _env: env,
                prev,
                root,
            }
        }
    }

    impl Drop for GraphRootPin {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var(super::REPO_GRAPH_ROOT_ENV, v),
                None => std::env::remove_var(super::REPO_GRAPH_ROOT_ENV),
            }
        }
    }
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

    /// The consumer-facing literals, pinned. The in-tree spelling is what a checkout's `.codegraph/`
    /// is recognised by (and reported as); the root dirname is the state-home registry entry; the
    /// env name is the documented override. Changing any is a coordinated release, not a rename.
    #[test]
    fn the_spellings_are_the_ones_consumers_expect() {
        assert_eq!(CODE_GRAPH_DB_REL, ".codegraph/estate.db");
        assert_eq!(IN_TREE_CODE_GRAPH_DIR, ".codegraph");
        // The production directory spelling and the test stand-in file spelling are ONE shape.
        assert_eq!(
            CODE_GRAPH_DB_REL,
            format!("{IN_TREE_CODE_GRAPH_DIR}/{CODE_GRAPH_DB_FILE}")
        );
        assert_eq!(CODE_GRAPH_DB_FILE, "estate.db");
        assert_eq!(REPO_GRAPH_ROOT_ENV, "WICKED_ESTATE_REPO_GRAPH_ROOT");
        assert_eq!(REPO_GRAPHS_DIRNAME, "repo-graphs");
        assert_eq!(LEGACY_ESTATE_HOME_DIRNAME, ".wicked-estate");
        // The registry entry the fence classifies `<state home>/repo-graphs` by MUST exist, or
        // every governed launch on a daemon that has indexed a repo is refused by name.
        assert!(
            crate::state_home::registry()
                .expect("the embedded registry parses")
                .classify(REPO_GRAPHS_DIRNAME)
                .is_some(),
            "`{REPO_GRAPHS_DIRNAME}` must be registered in tests/fixtures/state-home-subtrees.json"
        );
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
        assert_eq!(
            in_tree_code_graph_dir(Path::new("/repo")),
            Path::new("/repo").join(".codegraph")
        );
    }

    /// Precedence 1–3 of the module ADR, pure: the override wins outright; a bound state home
    /// puts the root at `<state home>/repo-graphs`; an unbound thread gets the DEFAULT state home
    /// `<home>/.wicked-crew/repo-graphs`; nothing at all resolves to nothing. The legacy estate
    /// home is NEVER an answer — it is the migration source only.
    #[test]
    fn the_root_is_the_override_then_the_state_home_then_the_default_state_home() {
        let over = Some(std::ffi::OsString::from("/x/graphs"));
        let home = Some(std::ffi::OsString::from("/home/u"));
        let sh = PathBuf::from("/srv/crew-state");
        assert_eq!(
            repo_graph_root_from(over.clone(), Some(&sh), home.clone()),
            Some(PathBuf::from("/x/graphs")),
            "the env override wins outright, even over a bound state home"
        );
        assert_eq!(
            repo_graph_root_from(None, Some(&sh), home.clone()),
            Some(sh.join("repo-graphs")),
            "a bound state home puts the root at <state home>/repo-graphs"
        );
        assert_eq!(
            repo_graph_root_from(None, None, home.clone()),
            Some(
                Path::new("/home/u")
                    .join(".wicked-crew")
                    .join("repo-graphs")
            ),
            "no binding falls back to the DEFAULT state home — never the estate home"
        );
        assert_eq!(
            repo_graph_root_from(Some(std::ffi::OsString::new()), None, None),
            None,
            "an EMPTY override does not name a root, and no home resolves to nothing"
        );
        assert_eq!(
            legacy_repo_graph_root_from(home),
            Some(
                Path::new("/home/u")
                    .join(".wicked-estate")
                    .join("repo-graphs")
            ),
            "the legacy root is the pre-#406 estate home"
        );
    }

    /// The store path → root derivation the launchers and out-of-process consumers use: the
    /// canonical parent of `--db`, plus `repo-graphs`. `:memory:` names no directory.
    #[test]
    fn the_root_for_a_store_is_its_parent_plus_repo_graphs() {
        // Holds the read side of the env lock: the override would outrank the derivation.
        let _env = REPO_GRAPH_ROOT_ENV_LOCK
            .read()
            .unwrap_or_else(|p| p.into_inner());
        if std::env::var_os(REPO_GRAPH_ROOT_ENV).is_some_and(|v| !v.is_empty()) {
            eprintln!("skipping: {REPO_GRAPH_ROOT_ENV} is set in this process");
            return;
        }
        let state_home = scratch("root-for-store");
        let db = state_home.join("core.db");
        let canonical = std::fs::canonicalize(&state_home).unwrap();
        assert_eq!(
            repo_graph_root_for_store(db.to_str().unwrap()),
            Some(canonical.join("repo-graphs"))
        );
        // Through a bound scope on THIS thread, the same answer — and it is undone on drop.
        let before = repo_graph_root();
        {
            let _scope = StateHomeScope::for_store(db.to_str().unwrap());
            assert_eq!(bound_state_home(), Some(canonical.clone()));
            assert_eq!(repo_graph_root(), Some(canonical.join("repo-graphs")));
        }
        assert_eq!(bound_state_home(), None, "the scope unbinds on drop");
        assert_eq!(repo_graph_root(), before);
        // A store that names no directory binds nothing, so the default applies.
        let _scope = StateHomeScope::for_store(":memory:");
        assert_eq!(bound_state_home(), None);
        let _ = std::fs::remove_dir_all(&state_home);
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

    /// core#406, the headline: a checkout carrying `<repo>/.codegraph/estate.db` (a graph an older
    /// engine indexed in-tree, or one its git history TRACKS) is NEVER adopted — not for
    /// resolution, not for read, not for write. The record resolves under the live root; the read
    /// side answers `None` until something indexes THERE; the write side mints THERE; the in-tree
    /// file is neither read nor touched — it is only recognised, for the repo-card finding.
    #[test]
    fn an_in_tree_graph_is_never_adopted_for_resolution_read_or_write() {
        let base = scratch("never-in-tree");
        let repo = base.join("repo");
        let in_tree = repo.join(code_graph_rel());
        std::fs::create_dir_all(in_tree.parent().unwrap()).unwrap();
        std::fs::write(&in_tree, b"185 MB of graph, notionally, tracked by git").unwrap();
        let in_tree_before = std::fs::metadata(&in_tree).unwrap().modified().unwrap();
        let root = base.join("state-home").join("repo-graphs");
        let want = repo_graph_db_at(&root, &repo);

        assert_eq!(
            resolved_code_graph_db_at(&repo, Some(&root)),
            Some(want.clone()),
            "resolution is the live root, whatever sits in the tree"
        );
        assert_eq!(
            existing_code_graph_at(&repo, Some(&root)),
            None,
            "the READ path does not see the in-tree file — nothing under the root has been indexed"
        );
        assert_eq!(
            code_graph_path_for_write_at(&repo, Some(&root)).unwrap(),
            want,
            "the WRITE path mints under the root — never refreshes the in-tree file"
        );
        assert!(want.parent().unwrap().is_dir(), "the key dir is created");
        assert!(
            has_in_tree_code_graph(&repo),
            "…and the in-tree directory IS recognised, so the record can report it"
        );
        assert!(!has_in_tree_code_graph(&base.join("clean")));
        assert_eq!(
            std::fs::metadata(&in_tree).unwrap().modified().unwrap(),
            in_tree_before,
            "the customer's file is untouched"
        );
        // Once the indexer writes under the root, the read side finds it there.
        std::fs::write(&want, b"indexed").unwrap();
        assert_eq!(
            existing_code_graph_at(&repo, Some(&root)).as_deref(),
            Some(want.as_path())
        );
        // No root at all: NOTHING resolves — never a fallback into the tree (the pre-#406
        // behaviour), and the write side is an error rather than a path.
        assert_eq!(resolved_code_graph_db_at(&repo, None), None);
        assert_eq!(existing_code_graph_at(&repo, None), None);
        let err = code_graph_path_for_write_at(&repo, None).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(err.to_string().contains(REPO_GRAPH_ROOT_ENV), "{err}");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A repo with no in-tree graph mints under the root — the key dir is created, the working
    /// tree stays untouched, and the read side still answers `None` until something indexes.
    #[test]
    fn a_fresh_repo_mints_under_the_root_and_leaves_the_tree_clean() {
        let base = scratch("mint");
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let root = base.join("repo-graphs");

        let want = repo_graph_db_at(&root, &repo);
        assert_eq!(
            resolved_code_graph_db_at(&repo, Some(&root)),
            Some(want.clone())
        );
        assert_eq!(
            existing_code_graph_at(&repo, Some(&root)),
            None,
            "no graph anywhere ⇒ None — never a path to a database nothing wrote (FINDING-069)"
        );
        let for_write = code_graph_path_for_write_at(&repo, Some(&root)).unwrap();
        assert_eq!(for_write, want);
        assert!(want.parent().unwrap().is_dir(), "the key dir is created");
        assert!(
            !in_tree_code_graph_dir(&repo).exists(),
            "the working tree is NOT polluted — that is the whole point"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The env override, END TO END through the public (env-reading) resolvers, over a bound state
    /// home: everything lands under the override, and neither the bound state home's root nor the
    /// real default home's key dir for this repo is ever created. Holds the crate-wide write lock;
    /// every other resolver test injects its root and never reads env.
    #[test]
    fn the_env_override_redirects_the_root_away_from_every_home() {
        let _env = REPO_GRAPH_ROOT_ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var_os(REPO_GRAPH_ROOT_ENV);

        let base = scratch("env-override");
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let over = base.join("override");
        std::env::set_var(REPO_GRAPH_ROOT_ENV, &over);
        let state_home = base.join("state-home");
        std::fs::create_dir_all(&state_home).unwrap();
        let _scope = StateHomeScope::for_store(state_home.join("core.db").to_str().unwrap());

        let resolved = resolved_code_graph_db(&repo).expect("a root resolves");
        let for_write = code_graph_path_for_write(&repo).unwrap();
        assert_eq!(resolved, repo_graph_db_at(&over, &repo));
        assert_eq!(for_write, resolved);
        assert!(resolved.starts_with(&over), "{resolved:?}");
        assert!(
            !state_home.join("repo-graphs").exists(),
            "the override outranks the bound state home"
        );
        if let Some(default_root) = repo_graph_root_from(None, None, home_dir()) {
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

    /// The shape recognition the sandbox grants key off: exactly `<root>/<key>/estate.db` → the
    /// key dir; everything else — the legacy in-tree shape included — is NOT a graph (no grant).
    #[test]
    fn classification_recognizes_only_the_state_home_shape() {
        let base = scratch("classify");
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let root = base.join("repo-graphs");
        let key = repo_graph_key(&repo);

        let db = repo_graph_db_at(&root, &repo);
        assert_eq!(
            classify_code_graph_db_at(&db, Some(&root)),
            Some(root.join(&key)),
            "the live shape classifies to EXACTLY the key dir"
        );

        for (why, bad) in [
            (
                "the legacy in-tree shape — a graph is never in the tree (core#406)",
                repo.join(code_graph_rel()),
            ),
            ("relative", PathBuf::from("repo").join(code_graph_rel())),
            ("wrong filename", root.join(&key).join("other.db")),
            ("db directly under the root", root.join(CODE_GRAPH_DB_FILE)),
            (
                "key dir under a DIFFERENT root",
                base.join("elsewhere").join(&key).join(CODE_GRAPH_DB_FILE),
            ),
            (
                "traversal-shaped key segment",
                root.join("..").join(CODE_GRAPH_DB_FILE),
            ),
        ] {
            assert!(
                classify_code_graph_db_at(&bad, Some(&root)).is_none(),
                "{why} must not classify as a graph: {bad:?}"
            );
        }
        // And with NO root resolvable, nothing classifies — not even the live shape.
        assert!(classify_code_graph_db_at(&db, None).is_none());

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A real SQLite db with `n` rows in `t`, as the indexer would leave one (WAL mode). The
    /// writer connection is RETURNED and kept open by the caller, so the rows sit in an
    /// un-checkpointed WAL while another connection still holds the db — the shape a live daemon's
    /// graph has, and the case a byte copy of `estate.db` alone would get wrong.
    fn sqlite_with_rows(path: &Path, n: usize) -> rusqlite::Connection {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let c = rusqlite::Connection::open(path).unwrap();
        c.pragma_update(None, "journal_mode", "WAL").unwrap();
        c.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        c.execute_batch("CREATE TABLE t (v INTEGER)").unwrap();
        for i in 0..n {
            c.execute("INSERT INTO t (v) VALUES (?1)", [i as i64])
                .unwrap();
        }
        c
    }

    fn row_count(path: &Path) -> i64 {
        let c =
            rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        c.query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap()
    }

    /// The boot-time migration: a registered repo whose graph sits under the legacy estate home and
    /// nowhere under the live root is copied ONCE (page-consistent, WAL included), the source is
    /// left in place, a second boot is a silent no-op, a repo already under the root is never
    /// overwritten, and a repo with no legacy graph produces nothing.
    #[test]
    fn migration_copies_a_legacy_graph_once_and_leaves_the_source_in_place() {
        let base = scratch("migrate");
        let legacy_root = base.join(".wicked-estate").join("repo-graphs");
        let root = base.join("state-home").join("repo-graphs");
        let alpha = base.join("alpha");
        let beta = base.join("beta");
        let gamma = base.join("gamma");
        for r in [&alpha, &beta, &gamma] {
            std::fs::create_dir_all(r).unwrap();
        }
        // alpha: legacy only → migrates. beta: legacy AND live → the live one wins, untouched.
        // gamma: never indexed → nothing.
        let _alpha_writer = sqlite_with_rows(&repo_graph_db_at(&legacy_root, &alpha), 7);
        let _beta_legacy_writer = sqlite_with_rows(&repo_graph_db_at(&legacy_root, &beta), 3);
        let _beta_live_writer = sqlite_with_rows(&repo_graph_db_at(&root, &beta), 1);
        let beta_live_before = std::fs::read(repo_graph_db_at(&root, &beta)).unwrap();
        let repos = [
            ("alpha", alpha.as_path()),
            ("beta", beta.as_path()),
            ("gamma", gamma.as_path()),
        ];

        let out = migrate_legacy_repo_graphs_at(repos, &root, &legacy_root);
        assert_eq!(
            out,
            vec![GraphMigration::Copied {
                repo_id: "alpha".into(),
                from: repo_graph_db_at(&legacy_root, &alpha),
                to: repo_graph_db_at(&root, &alpha),
            }],
            "exactly alpha migrates: {out:?}"
        );
        assert_eq!(
            row_count(&repo_graph_db_at(&root, &alpha)),
            7,
            "the copy is page-consistent — the un-checkpointed WAL rows arrived too, with the \
             source still open"
        );
        assert!(
            repo_graph_db_at(&legacy_root, &alpha).is_file(),
            "the source is left in place — the engine deletes nothing it did not write"
        );
        assert_eq!(
            std::fs::read(repo_graph_db_at(&root, &beta)).unwrap(),
            beta_live_before,
            "a graph already under the root is never overwritten"
        );
        assert!(
            !root.join(repo_graph_key(&gamma)).exists(),
            "a repo with no legacy graph mints nothing"
        );
        assert!(out[0].notice().contains("alpha") && out[0].notice().contains("core#406"));

        // Second boot: nothing left to do.
        assert_eq!(
            migrate_legacy_repo_graphs_at(repos, &root, &legacy_root),
            vec![]
        );
        // The override pointed at the legacy directory itself: nothing to move, nothing touched.
        assert_eq!(
            migrate_legacy_repo_graphs_at(repos, &legacy_root, &legacy_root),
            vec![]
        );
        // A source that is not a database fails CLOSED: reported, the torn destination removed.
        let delta = base.join("delta");
        std::fs::create_dir_all(&delta).unwrap();
        let bad = repo_graph_db_at(&legacy_root, &delta);
        std::fs::create_dir_all(bad.parent().unwrap()).unwrap();
        std::fs::write(&bad, b"not a sqlite database").unwrap();
        let out = migrate_legacy_repo_graphs_at([("delta", delta.as_path())], &root, &legacy_root);
        assert!(
            matches!(&out[..], [GraphMigration::Failed { repo_id, .. }] if repo_id == "delta"),
            "{out:?}"
        );
        assert!(
            !root.join(repo_graph_key(&delta)).exists(),
            "a failed copy leaves no torn destination for a worker to open"
        );
        assert!(bad.is_file(), "…and the source is untouched");

        let _ = std::fs::remove_dir_all(&base);
    }
}
