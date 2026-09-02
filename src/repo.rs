//! REPO REGISTRY — first-class, persistent registration of the git repositories the orchestrator
//! works within, plus the git-worktree isolation a run uses so the user's working tree is never
//! touched.
//!
//! A [`RepoEntry`] is a `Node(Other("repo_entry"))` on the shared estate store (mirrors the
//! `AgentSession` projection in [`crate::domain`]). A run that targets a registered repo gets its own
//! worktree at `<repo>/wicked-worktrees/<id>` on branch `wicked/<id>`, where `<id>` is the run id's
//! ref- and filesystem-safe spelling ([`sanitize_worktree_id`], core#337 — a campaign node's run id
//! carries `:`, illegal in a git ref and on NTFS); the worker runs there
//! (augment mode — see `ORCHESTRATOR.md` §4). Worktrees are reaped on a terminal run status — but
//! only when CLEAN ([`reap_worktree_if_clean`]): a tree holding uncommitted work is kept and logged,
//! never force-deleted, because those bytes may be the only copy of the work. The startup orphan
//! reaper ([`reap_orphan_worktrees`]) applies the same rule to terminal runs' leftovers and
//! force-removes only worktrees whose run id no longer exists on the store at all. The
//! `wicked/<run_id>` BRANCH is never deleted by any of this — the branch is the durable record of a
//! run's landed work; the worktree is scaffolding.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use wicked_apps_core::{
    synthetic_symbol, FromNode, GraphRead, GraphStore, HardenedCommand, Language, Location, Node,
    NodeKind, Span, ToNode, SYMBOL_SCHEME,
};
use wicked_estate_core::SymbolQuery;

use crate::domain::put_node;

/// Node-kind for a registered repository.
pub const REPO_ENTRY: &str = "repo_entry";

/// A registered repository the orchestrator can run within.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoEntry {
    /// Stable id (slug of the name) — the node identity.
    pub id: String,
    /// Human display name.
    pub name: String,
    /// Absolute path to the git repository root.
    pub root_path: String,
    /// The branch worktrees are based on (resolved at registration).
    pub default_branch: String,
    /// Registration timestamp (unix seconds), supplied by the caller (no wall-clock in the lib).
    #[serde(default)]
    pub registered_at: i64,
    /// ABSOLUTE path of this repo's code graph. **Derived, never authoritative in the record.**
    ///
    /// It exists so out-of-process consumers stop re-deriving it. crew spelled
    /// `join(root_path, '.codegraph', 'estate.db')` in five places against the engine's own sixth
    /// spelling, and nothing failed when they disagreed — the worker just queried an empty database
    /// (FINDING-069). A field on the record they already read makes the engine the one source.
    ///
    /// [`RepoEntry::from_node`] recomputes it from `root_path` and discards whatever was persisted, so
    /// a record written before this field existed reads back correct, and a repo that moves does not
    /// carry a stale path forward. Do not write to it expecting it to stick.
    ///
    /// TWO SHAPES can appear here, decided by the resolver in `code_graph.rs` (see its ADR): a repo
    /// that already has an in-tree `<root>/.codegraph/estate.db` keeps publishing that (continuity —
    /// an indexed repo never migrates silently); a repo without one publishes its estate-home path
    /// (`<estate_root>/<key>/estate.db`), so fresh registrations stop polluting working trees.
    #[serde(default)]
    pub code_graph_db: String,
}

impl ToNode for RepoEntry {
    fn node_kind() -> &'static str {
        REPO_ENTRY
    }
    fn to_node(&self) -> Node {
        let mut node = Node::new(
            synthetic_symbol(REPO_ENTRY, &self.id),
            NodeKind::Other(REPO_ENTRY.to_string()),
            self.id.clone(),
            Language::new(SYMBOL_SCHEME),
            Location::new(format!("{REPO_ENTRY}/{}", self.id), Span::ZERO),
        );
        if let serde_json::Value::Object(map) =
            serde_json::to_value(self).expect("RepoEntry serializes to JSON")
        {
            node.metadata = map;
        }
        node
    }
}

impl FromNode for RepoEntry {
    fn from_node(node: &Node) -> anyhow::Result<Self> {
        match &node.kind {
            NodeKind::Other(k) if k == REPO_ENTRY => {}
            other => anyhow::bail!("expected NodeKind::Other({REPO_ENTRY:?}), got {other:?}"),
        }
        let mut entry: RepoEntry =
            serde_json::from_value(serde_json::Value::Object(node.metadata.clone()))
                .map_err(|e| anyhow::anyhow!("node {} is not a valid RepoEntry: {e}", node.name))?;
        entry.code_graph_db = code_graph_db(&entry.root_path);
        Ok(entry)
    }
}

/// This repo's code-graph path, absolute, derived from its root through the engine's ONE resolver
/// (`code_graph::resolved_code_graph_db`: legacy in-tree when the repo already has one, else the
/// estate home). The only spelling any consumer needs.
fn code_graph_db(root_path: &str) -> String {
    crate::code_graph::resolved_code_graph_db(Path::new(root_path))
        .to_string_lossy()
        .into_owned()
}

/// What a caller asks to register. The id/branch are resolved by [`register_repo`].
#[derive(Debug, Clone)]
pub struct RepoSpec {
    pub name: String,
    pub root_path: String,
    pub registered_at: i64,
}

/// A 4-word kebab slug of `name` (mirrors the UI's slug, minus the timestamp suffix).
fn slug(name: &str) -> String {
    let base: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let words: Vec<&str> = base.split('-').filter(|w| !w.is_empty()).take(4).collect();
    if words.is_empty() {
        "repo".to_string()
    } else {
        words.join("-")
    }
}

/// Run `git -C <root> <args...>` and return `(success, stdout, stderr)`.
fn git(root: &str, args: &[&str]) -> anyhow::Result<(bool, String, String)> {
    let out = Command::new("git")
        .hardened()
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("git could not run: {e}"))?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
    ))
}

/// Validate `root` is a git repo with at least one commit (a worktree requires a base commit), and
/// return its current branch name.
pub fn validate_git_repo(root: &str) -> anyhow::Result<String> {
    if !Path::new(root).is_dir() {
        anyhow::bail!("{root} is not a directory");
    }
    let (ok, _, _) = git(root, &["rev-parse", "--is-inside-work-tree"])?;
    if !ok {
        anyhow::bail!("{root} is not a git repository");
    }
    let (has_commit, _, _) = git(root, &["rev-parse", "HEAD"])?;
    if !has_commit {
        anyhow::bail!("{root} has no commits yet (a worktree needs at least one commit)");
    }
    let (_, branch, _) = git(root, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    Ok(if branch.is_empty() {
        "main".to_string()
    } else {
        branch
    })
}

/// Register a repository: validate it, resolve its id + default branch, persist the [`RepoEntry`].
pub fn register_repo(store: &mut dyn GraphStore, spec: RepoSpec) -> anyhow::Result<RepoEntry> {
    // Resolve the root to an ABSOLUTE path before persisting. A caller may register with a relative
    // path (`register-repo --path ./repo`), but the daemon and every downstream consumer — worktree
    // creation, code-graph resolution — run from a DIFFERENT cwd and assume the stored root_path is
    // absolute. Persisting the as-given relative path yields a root_path (and a code_graph_db derived
    // from it) that resolves to nothing outside the registering cwd (core#214).
    //
    // `std::path::absolute`, NOT `std::fs::canonicalize`: canonicalize resolves symlinks and, on
    // Windows, returns a `\\?\C:\…` extended-length path that `git worktree add` rejects (breaking
    // create_worktree), and on macOS rewrites `/var`→`/private/var` — a spelling change the registry
    // contract does not want (a registered absolute path must round-trip verbatim). `absolute` makes
    // the path absolute and lexically normalises it WITHOUT touching the filesystem, so an already-
    // absolute root is preserved while a relative one is anchored to the cwd, on every platform.
    // validate_git_repo still runs first for its friendly "not a git repository" error.
    let default_branch = validate_git_repo(&spec.root_path)?;
    let root_path = std::path::absolute(&spec.root_path)
        .map_err(|e| {
            anyhow::anyhow!(
                "cannot resolve repo path {} to an absolute path: {e}",
                spec.root_path
            )
        })?
        .to_string_lossy()
        .into_owned();
    let entry = RepoEntry {
        id: slug(&spec.name),
        name: spec.name,
        code_graph_db: code_graph_db(&root_path),
        root_path,
        default_branch,
        registered_at: spec.registered_at,
    };
    put_node(store, entry.to_node())?;
    Ok(entry)
}

/// Every registered repo on the store.
pub fn list_repos(store: &dyn GraphRead) -> anyhow::Result<Vec<RepoEntry>> {
    let query = SymbolQuery {
        kinds: vec![NodeKind::Other(REPO_ENTRY.to_string())],
        ..Default::default()
    };
    Ok(store
        .find_symbols(&query)?
        .iter()
        .filter_map(|n| RepoEntry::from_node(n).ok())
        .collect())
}

/// Read one registered repo by id.
pub fn get_repo(store: &dyn GraphRead, repo_id: &str) -> anyhow::Result<Option<RepoEntry>> {
    match store.get_node(&synthetic_symbol(REPO_ENTRY, repo_id))? {
        Some(node) => Ok(Some(RepoEntry::from_node(&node)?)),
        None => Ok(None),
    }
}

/// Front-half coverage for ONE registered repo, computed over that repo's OWN code graph.
///
/// `recompute_front_half_coverage` run against the daemon store (`~/.wicked-crew/core.db`) is
/// meaningless: that store holds run/governance nodes but none of a repo's domain/requirement nodes,
/// so it reports a vacuous `coverage: 1.0` over an empty denominator and cannot name a repo
/// (FINDING-009). This resolves `repo_ref` from the registry on `daemon`, opens its `code_graph_db`
/// (the engine-resolved path every consumer shares — in-tree for a legacy-indexed repo, the estate
/// home otherwise; see `code_graph.rs`) READ-ONLY, and recomputes over THAT. An unknown `repo_ref`
/// is an error, never a silent vacuous report.
pub fn coverage_report_for_repo(
    daemon: &dyn GraphRead,
    repo_ref: &str,
) -> anyhow::Result<wicked_governance::CoverageReport> {
    let repo = get_repo(daemon, repo_ref)?
        .ok_or_else(|| anyhow::anyhow!("no registered repo '{repo_ref}'"))?;
    let repo_store = wicked_apps_core::open_store_ro(Some(repo.code_graph_db.as_str()))?;
    wicked_governance::recompute_front_half_coverage(&repo_store)
}

/// A summary of ONE registered repo's code graph — node counts by kind — read over that repo's OWN
/// graph store (its engine-resolved `code_graph_db`), never the daemon store (same discipline as
/// [`coverage_report_for_repo`]; FINDING-009/067/122). This is the read half of #122's web surface:
/// the studio shows what the estate graph holds for a repo instead of the operator wondering whether
/// it was ever populated. An unknown `repo_ref` is an ERROR, never a silent empty summary.
pub fn graph_kinds_for_repo(
    daemon: &dyn GraphRead,
    repo_ref: &str,
) -> anyhow::Result<Vec<(String, usize)>> {
    let repo = get_repo(daemon, repo_ref)?
        .ok_or_else(|| anyhow::anyhow!("no registered repo '{repo_ref}'"))?;
    // `graph_kinds` opens the store read-only itself (it takes the db path), so this stays a thin
    // resolve-repo → delegate, exactly like the coverage path above.
    crate::graph_browser::graph_kinds(repo.code_graph_db.as_str())
}

/// The directory NEW worktrees for `repo_root` are created under (crew#276): a NON-dotted
/// path. Worktrees used to live at `.wicked/worktrees/`, which put a dot-segment into every
/// absolute path inside the run — and dotfile-sensitive behavior in the repo under test broke
/// purely from location (express `send` 404s any path containing a dot-segment; two
/// wicked-interactive export tests failed in every governed worktree). A worker diagnosing
/// phantom failures the operator can't reproduce is a workflow hazard, not a cosmetic one.
fn worktrees_root(repo_root: &str) -> PathBuf {
    Path::new(repo_root).join("wicked-worktrees")
}

/// Where worktrees lived before crew#276. Runs created under the old layout must keep resuming
/// and reaping — their sessions carry absolute workdirs, but the (repo, run_id) → path derivation
/// in this module must find them too.
fn legacy_worktrees_root(repo_root: &str) -> PathBuf {
    Path::new(repo_root).join(".wicked").join("worktrees")
}

/// The 64-bit FNV-1a hash of `s` — small, dependency-free, and (unlike `DefaultHasher`) stable
/// across platforms and Rust releases. Stability is load-bearing: the hash lands in on-disk
/// directory names and git branch names that resume/reap must re-derive after an engine upgrade.
fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// Windows reserved device filenames (matched case-insensitively against the name up to its first
/// `.`): a directory named `NUL` or `con.anything` is unusable on NTFS even though every char in
/// it is individually legal.
fn is_windows_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    let up = stem.to_ascii_uppercase();
    matches!(up.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (up.len() == 4
            && (up.starts_with("COM") || up.starts_with("LPT"))
            && up.as_bytes()[3].is_ascii_digit()
            && up.as_bytes()[3] != b'0')
}

/// core#337 — the ref- and filesystem-safe spelling of `run_id`, used for BOTH the worktree
/// directory name and the branch component after `wicked/`.
///
/// A campaign node's run id is `{campaign}:{node}:a{attempt}` ([`crate::campaign`] §2.1) — and `:`
/// is illegal both in a git ref and in an NTFS file name, so the verbatim spelling failed every
/// repo-scoped campaign node at dispatch ("is not a valid branch name"). The mapping is a pure
/// function of the id (resume, reap and the startup orphan reaper all re-derive it) and tiered:
///
///  - **nothing illegal** → returned byte-for-byte, so plain uuid run ids are untouched;
///  - **`:` is the only illegal content** → plain `':' → '-'`, NO hash suffix. This deliberately
///    matches the convention crew's shipped workaround already stamps on operators' repos
///    (branch `wicked/<id with ':' → '-'>` — crew#390/#391,
///    `packages/crew/src/campaigns/worktrees.ts`), so engine and daemon spell the SAME branch;
///  - **anything else** — other ref-/NTFS-illegal chars (per `git check-ref-format`: control
///    chars, space, `~ ^ ? * [ \ < > | "`, and `/` since this is one component), the sequences
///    `..` and `@{`, a leading or trailing `.`, a `.lock` suffix, a lone `@`, or an NTFS reserved
///    device name — → mapped to `-` plus an 8-hex FNV-1a suffix of the RAW id, so two distinct
///    ids that flatten to the same string get distinct worktrees instead of silently sharing one.
///
/// The output is always its own fixed point (`sanitize(sanitize(x)) == sanitize(x)`): the startup
/// reaper re-derives ids from directory NAMES, which may already be sanitized spellings.
pub(crate) fn sanitize_worktree_id(run_id: &str) -> String {
    let mut mapped = String::with_capacity(run_id.len());
    let mut colon_mapped = false; // some ':' was replaced
    let mut residual = false; // anything OTHER than ':' had to change
    let mut prev = '\0';
    for c in run_id.chars() {
        let illegal = c.is_ascii_control()
            || matches!(
                c,
                ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\' | '/' | '<' | '>' | '|' | '"'
            )
            || (c == '.' && prev == '.') // ".." is illegal in a ref component
            || (c == '{' && prev == '@'); // "@{" is illegal anywhere in a ref
        if illegal {
            if c == ':' {
                colon_mapped = true;
            } else {
                residual = true;
            }
            mapped.push('-');
            prev = '-';
        } else {
            mapped.push(c);
            prev = c;
        }
    }
    // Position rules a char map can't express: a ref component cannot start or end with '.' and
    // cannot end with ".lock"; NTFS additionally refuses a trailing '.' and reserved device names.
    if mapped.starts_with('.') {
        mapped.replace_range(..1, "-");
        residual = true;
    }
    if mapped.ends_with('.') {
        let n = mapped.len();
        mapped.replace_range(n - 1.., "-");
        residual = true;
    }
    if let Some(stripped) = mapped.strip_suffix(".lock") {
        mapped = format!("{stripped}-lock");
        residual = true;
    }
    if mapped == "@" || mapped.is_empty() || is_windows_reserved(&mapped) {
        residual = true;
    }
    if residual {
        format!("{mapped}-{:08x}", (fnv1a64(run_id) >> 32) as u32)
    } else if colon_mapped {
        mapped
    } else {
        run_id.to_string()
    }
}

/// The on-disk path of `run_id`'s worktree: `<new root>/<sanitized id>`, unless the run already
/// exists at an earlier spelling — the legacy `.wicked/worktrees` root (resume/reap of a pre-move
/// run), or the RAW id under the new root (crew#390/#391: crew's shipped workaround
/// pre-provisions campaign-node worktrees at `wicked-worktrees/<raw id>` and rides the documented
/// reuse contract, so the derivation must FIND them; a raw spelling that needed sanitizing can
/// only exist on filesystems that allow it, so the probe is inert on Windows).
fn worktree_path(repo_root: &str, run_id: &str) -> PathBuf {
    let legacy = legacy_worktrees_root(repo_root).join(run_id);
    if legacy.is_dir() {
        return legacy;
    }
    let sanitized = sanitize_worktree_id(run_id);
    if sanitized != run_id {
        let raw = worktrees_root(repo_root).join(run_id);
        if raw.is_dir() {
            return raw;
        }
    }
    worktrees_root(repo_root).join(sanitized)
}

/// Keep the operator's `git status` clean in the parent checkout: the new worktree dir is not
/// dotted, so exclude it via `.git/info/exclude` (repo-local, never touches the tracked
/// `.gitignore`). Idempotent; best-effort — a failure to write the exclude is cosmetic.
fn ensure_worktrees_excluded(repo_root: &str) {
    const ENTRY: &str = "wicked-worktrees/";
    let exclude = Path::new(repo_root)
        .join(".git")
        .join("info")
        .join("exclude");
    let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == ENTRY) {
        return;
    }
    if let Some(parent) = exclude.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(ENTRY);
    content.push('\n');
    let _ = std::fs::write(&exclude, content);
}

/// Is `wt` a live git worktree, rather than merely a directory that sits where one should?
///
/// A worktree carries a `.git` **file** (not a directory) pointing at its admin dir, and `rev-parse`
/// inside it resolves. Both are checked: the file alone can outlive the admin entry that gives it
/// meaning, and `rev-parse` alone succeeds anywhere beneath the parent repo — including an empty
/// `.wicked/worktrees/<id>/`, which is exactly the case this exists to reject.
///
/// `pub(crate)` because the resume path needs the same test: `reprovision_reaped_worktree` decides
/// whether a run's recorded workdir still IS a checkout, and a `is_dir()` there re-opens FINDING-059
/// one level up from where [`create_worktree`] closed it.
pub(crate) fn is_live_worktree(wt: &Path) -> bool {
    if !wt.join(".git").is_file() {
        return false;
    }
    let Some(p) = wt.to_str() else { return false };
    matches!(git(p, &["rev-parse", "--git-dir"]), Ok((true, _, _)))
}

// ── Worktree ownership (core#337, the collision half) ────────────────────────────────────────────
//
// The colon tier of [`sanitize_worktree_id`] is deliberately hashless (crew#390/#391 branch
// parity), so two DISTINCT raw run ids can spell the SAME worktree name: `a-b:c:a0` and
// `a:b-c:a0` both flatten to `a-b-c-a0`. Names alone therefore cannot keep colliding runs apart —
// ownership does. Each minted (or adopted) worktree carries the RAW run id it belongs to in a
// marker file inside its git ADMIN directory (`<repo>/.git/worktrees/<name>/wicked-run-id` —
// never the working tree, so `git status` stays clean, and the marker dies with the worktree on
// remove/prune). `create_worktree` refuses to hand a marked tree to a different raw id, and the
// destructive forms (`remove_worktree`, `reap_worktree_if_clean`) refuse to touch one — a
// colliding run's cancel/terminal cleanup derives the OWNER's path (actor.rs keys both on
// `repo_ref` alone), and without the guard it would reap or force-remove a live run's tree.

/// The worktree's git admin directory (`<repo>/.git/worktrees/<name>`) — resolvable only while
/// the tree is a live worktree.
fn worktree_admin_dir(wt: &Path) -> Option<PathBuf> {
    let p = wt.to_str()?;
    match git(p, &["rev-parse", "--absolute-git-dir"]) {
        Ok((true, out, _)) if !out.is_empty() => Some(PathBuf::from(out)),
        _ => None,
    }
}

/// The RAW run id a live worktree was minted (or adopted) for — `None` when unmarked: a tree from
/// a pre-marker engine, or one a downstream pre-provisioned (crew#390/#391).
fn worktree_owner(wt: &Path) -> Option<String> {
    let marker = worktree_admin_dir(wt)?.join("wicked-run-id");
    let owner = std::fs::read_to_string(marker).ok()?;
    let owner = owner.trim();
    if owner.is_empty() {
        return None;
    }
    Some(owner.to_string())
}

/// Record `run_id` as the worktree's owner. Best-effort: an unmarked tree degrades to the
/// pre-marker adopt-on-reuse behavior, it never fails a run.
fn stamp_worktree_owner(wt: &Path, run_id: &str) {
    if let Some(dir) = worktree_admin_dir(wt) {
        let _ = std::fs::write(dir.join("wicked-run-id"), run_id);
    }
}

/// Whether a DESTRUCTIVE operation keyed by `run_id` may touch the tree at `wt`. True when the
/// tree is unmarked (pre-marker mints, downstream-provisioned trees), or the marker matches the
/// key — either verbatim (a run cleaning its own tree) or via [`sanitize_worktree_id`] (the
/// startup reaper keys by directory NAME, which is the owner's sanitized spelling). False means
/// the tree belongs to a DIFFERENT raw run id that merely spells the same name — leave it alone.
fn may_touch_worktree(wt: &Path, run_id: &str) -> bool {
    match worktree_owner(wt) {
        None => true,
        Some(owner) => owner == run_id || sanitize_worktree_id(&owner) == run_id,
    }
}

/// Create an isolated git worktree for `run_id` at `<repo>/wicked-worktrees/<id>` on a fresh
/// `wicked/<id>` branch, where `<id>` is [`sanitize_worktree_id`]'s spelling of the run id
/// (core#337 — campaign run ids carry `:`, illegal in a git ref and on NTFS; plain ids pass
/// through byte-for-byte). Idempotent for a genuine resume: a live worktree already at the path is
/// reused — including one a downstream pre-provisioned at the RAW id spelling (crew#390/#391).
/// Reuse is OWNER-only: a live tree marked for a DIFFERENT raw run id that merely spells the same
/// sanitized name (the hashless colon tier) is refused loudly, never shared — see the worktree
/// ownership section above [`worktree_owner`]. Returns the worktree path.
///
/// The reuse test is [`is_live_worktree`], not `is_dir()`. It used to be `is_dir()`, and the
/// difference is FINDING-059: `remove_worktree` falls back to `remove_dir_all`, a partial removal
/// leaves the directory shell behind, and the `git worktree prune` that follows then deregisters the
/// admin entry *because* the path no longer has a `.git` file. The result is an empty, unregistered
/// directory sitting exactly where a worktree belongs — which `is_dir()` accepted and returned as an
/// isolated checkout. The worker handed one noticed ("the assigned worktree is an empty,
/// unregistered directory, so I'll work in the main repo checkout") and wrote 297 lines onto
/// `master` of the operator's real clone. A cwd is not a boundary; the worktree is, so its existence
/// has to be verified rather than inferred from a stat.
pub fn create_worktree(repo_root: &str, run_id: &str) -> anyhow::Result<PathBuf> {
    let wt = worktree_path(repo_root, run_id);
    if wt.is_dir() {
        if is_live_worktree(&wt) {
            // A genuine resume reuses the tree — but only the OWNER resumes. Two distinct raw
            // ids can spell the same worktree name (the hashless colon tier, core#337), and
            // handing one run's live tree to another would silently interleave their work.
            match worktree_owner(&wt) {
                Some(owner) if owner != run_id => anyhow::bail!(
                    "worktree {} already belongs to run {owner} — run {run_id}'s sanitized \
                     name collides with it (two distinct run ids can spell the same worktree \
                     name); refusing to share one working tree between two runs",
                    wt.display()
                ),
                Some(_) => {}
                // Unmarked: a pre-marker mint resolved FOR this id, or a tree a downstream
                // pre-provisioned for exactly this run (crew#390/#391) — adopt it.
                None => stamp_worktree_owner(&wt, run_id),
            }
            return Ok(wt); // genuine resume — reuse it
        }
        // Not a worktree. Recoverable only while it holds nothing: an empty shell can be cleared and
        // re-added, and `worktree add` accepts an existing empty directory anyway. Anything else is
        // a directory of unknown provenance, and calling it an isolated checkout is the failure
        // above — fail the run loudly instead of handing it over.
        // `unwrap_or(false)` reads an unreadable directory as NOT empty, so a permissions error
        // fails the run rather than silently taking the recovery branch on a tree we cannot see.
        let empty = std::fs::read_dir(&wt)
            .map(|mut d| d.next().is_none())
            .unwrap_or(false);
        if !empty {
            anyhow::bail!(
                "{} exists but is not a git worktree, and is not empty; refusing to run a unit \
                 against it (a worker given a non-checkout works in the parent repo instead)",
                wt.display()
            );
        }
        let _ = std::fs::remove_dir(&wt);
        // The other half of this state: git may still hold an admin entry for the path whose `.git`
        // file we just found missing, and `worktree add` refuses a path that is "already
        // registered". Prune drops exactly those dangling entries and touches no live worktree.
        let _ = git(repo_root, &["worktree", "prune"]);
    }
    std::fs::create_dir_all(worktrees_root(repo_root))?;
    ensure_worktrees_excluded(repo_root);
    let branch = format!("wicked/{}", sanitize_worktree_id(run_id));
    let wt_str = wt.to_string_lossy().to_string();
    let (ok, _, err) = git(repo_root, &["worktree", "add", &wt_str, "-b", &branch])?;
    if !ok {
        // A stale branch from a prior run can block re-add; retry without -b (reuse the branch).
        let (ok2, _, err2) = git(repo_root, &["worktree", "add", &wt_str, &branch])?;
        if !ok2 {
            anyhow::bail!("git worktree add failed: {err}{err2}");
        }
    }
    stamp_worktree_owner(&wt, run_id);
    Ok(wt)
}

/// Remove a run's worktree unconditionally (best-effort — a failure to clean up is logged, not
/// fatal). This is the DESTRUCTIVE form: `--force` deletes uncommitted work. It is reserved for the
/// two cases where discarding is the point — an operator's explicit Cancel (abandonment), and a
/// startup leftover whose run id no longer exists on the store (no record, nothing to preserve).
/// A run that merely FINISHED goes through [`reap_worktree_if_clean`] instead (FINDING-003).
pub fn remove_worktree(repo_root: &str, run_id: &str) {
    let wt = worktree_path(repo_root, run_id);
    // A colliding run id derives the OWNER's path (core#337, hashless colon tier) — and this is
    // the FORCE form, so touching a tree minted for a different raw id would destroy a live
    // run's work. The colliding run never held a tree here; there is nothing of its own to remove.
    if !may_touch_worktree(&wt, run_id) {
        eprintln!(
            "wicked-core: not removing worktree {} for run {run_id} — it belongs to a \
             different run whose id spells the same worktree name",
            wt.display()
        );
        return;
    }
    let wt_str = wt.to_string_lossy().to_string();
    let _ = git(repo_root, &["worktree", "remove", "--force", &wt_str]);
    // If git refused (e.g. already gone), drop the dir directly.
    if wt.is_dir() {
        let _ = std::fs::remove_dir_all(&wt);
    }
}

/// FINDING-003 — reap a TERMINAL run's worktree, but only when it is CLEAN. Returns whether the
/// path is gone.
///
/// Deliberately NOT `--force` and NO `remove_dir_all` fallback: git's non-forced `worktree remove`
/// refuses a tree with modified or untracked files, and that refusal is the safety property this
/// function is built on. A terminal run's uncommitted files are work that never landed on the
/// `wicked/<run_id>` branch (the known artifact-landing gap — 3 of the finding's 14 orphans carried
/// exactly that), so force-deleting them here would make the REAPER the thing that destroys the
/// only copy of the work. A kept tree is announced on stderr each time, so it is a visible,
/// named leftover rather than a silent leak; a clean tree adds nothing the branch doesn't already
/// carry, and goes. The branch itself is never touched either way.
pub fn reap_worktree_if_clean(repo_root: &str, run_id: &str) -> bool {
    let wt = worktree_path(repo_root, run_id);
    if !wt.is_dir() {
        // Nothing on disk. Drop any dangling admin entry so the path is re-usable.
        let _ = git(repo_root, &["worktree", "prune"]);
        return true;
    }
    // The tree at the derived path may belong to a DIFFERENT raw run id that spells the same
    // worktree name (core#337, hashless colon tier). A clean live tree is exactly what the
    // non-forced remove below would take, so the ownership check has to come first. `run_id`
    // itself never held a tree here — its reap is trivially complete.
    if !may_touch_worktree(&wt, run_id) {
        eprintln!(
            "wicked-core: not reaping worktree {} for run {run_id} — it belongs to a \
             different run whose id spells the same worktree name",
            wt.display()
        );
        return true;
    }
    let wt_str = wt.to_string_lossy().to_string();
    match git(repo_root, &["worktree", "remove", &wt_str]) {
        Ok((true, _, _)) => true,
        Ok((false, _, err)) => {
            eprintln!(
                "wicked-core: keeping worktree {} — git refused a non-forced remove ({}); it \
                 likely holds uncommitted work the wicked/{run_id} branch does not carry",
                wt.display(),
                err.trim()
            );
            false
        }
        Err(e) => {
            eprintln!("wicked-core: could not reap worktree {}: {e}", wt.display());
            false
        }
    }
}

/// Prune worktrees whose run is not live, on actor startup. For each
/// `<repo>/.wicked/worktrees/<id>`:
///  - `<id>` in `live_run_ids` (a session in a NON-terminal status) → kept, it may resume;
///  - `<id>` in `terminal_run_ids` (a session that finished) → [`reap_worktree_if_clean`] — the
///    same rule the terminal-status reap applies, re-run here so a crash between a run going
///    terminal and its reap (or a run predating the reap) converges on the next start instead of
///    surviving restarts forever (FINDING-003: 14 did);
///  - unknown to the store → [`remove_worktree`] (force): no session record exists, so there is no
///    run to resume and no outcome the leftover documents.
pub fn reap_orphan_worktrees(
    repos: &[RepoEntry],
    live_run_ids: &HashSet<String>,
    terminal_run_ids: &HashSet<String>,
) {
    // A worktree directory carries either a run id verbatim (plain ids; a raw-spelling campaign
    // tree crew pre-provisioned) or the id's SANITIZED spelling (core#337 — what create_worktree
    // mints for a campaign id). The store holds raw ids, so recognize both spellings here — or a
    // LIVE campaign run's checkout reads as "unknown to the store" and is force-removed at boot.
    let with_sanitized = |ids: &HashSet<String>| -> HashSet<String> {
        let mut all = ids.clone();
        all.extend(ids.iter().map(|id| sanitize_worktree_id(id)));
        all
    };
    let live_names = with_sanitized(live_run_ids);
    let terminal_names = with_sanitized(terminal_run_ids);
    for repo in repos {
        // Both layouts: pre-crew#276 leftovers under `.wicked/worktrees` reap by the same rules.
        for root in [
            worktrees_root(&repo.root_path),
            legacy_worktrees_root(&repo.root_path),
        ] {
            let Ok(entries) = std::fs::read_dir(&root) else {
                continue;
            };
            for e in entries.flatten() {
                if let Some(name) = e.file_name().to_str() {
                    if live_names.contains(name) {
                        continue;
                    }
                    if terminal_names.contains(name) {
                        let _ = reap_worktree_if_clean(&repo.root_path, name);
                    } else {
                        remove_worktree(&repo.root_path, name);
                    }
                }
            }
        }
        // Tidy git's worktree administrative list.
        let _ = git(&repo.root_path, &["worktree", "prune"]);
    }
}

// ── Worktree layout summary (FINDING-048) ────────────────────────────────────────────────────────

/// Directory names never worth a line in the summary: build output, vendored dependencies and
/// virtualenvs. They are large, uninformative, and present in nearly every repo.
const LAYOUT_NOISE: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    "out",
    "vendor",
    "__pycache__",
    "venv",
    "site-packages",
    "coverage",
];

/// Files that mark a directory as a project root. Their presence is the signal a worker needs: it is
/// what makes `autogpt_platform/backend` a place you can `cd` into and run something.
const PROJECT_MANIFESTS: &[&str] = &[
    "package.json",
    "pyproject.toml",
    "setup.py",
    "Cargo.toml",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "Gemfile",
    "composer.json",
    "CMakeLists.txt",
    "Makefile",
];

/// Total character budget. The prompt-composition audit behind FINDING-048 found task-specific text
/// was already only 5% of a 74.5k-char prompt; a layout that solves path-guessing by drowning the
/// task would trade one problem for a worse one.
const LAYOUT_BUDGET: usize = 1200;

/// Caps on breadth. A repo with 200 top-level entries is not made legible by listing all 200.
const MAX_TOP_LEVEL: usize = 32;
const MAX_CHILDREN: usize = 10;
/// Root files are the cheapest thing to rediscover (`ls`) and the least directional, so they get the
/// tightest cap — ragflow's 33 of them would otherwise be over half the map.
const MAX_ROOT_FILES: usize = 12;

/// Appended in place of whatever did not fit, so a reader can always tell a complete map from a
/// clipped one and knows the cheap way to get the rest.
pub(crate) const LAYOUT_TRUNCATED: &str = "; …truncated, run `ls` for the rest";

/// Joins one map entry to the next.
///
/// Named rather than written as a literal at the join site because its width is charged against the
/// budget: a hardcoded `2` at the accounting site and a `"; "` at the join site are free to drift,
/// and the first cut of this did exactly that — charging the separator for every entry when `join`
/// only writes one BETWEEN entries, so a map was billed 2 bytes it never spent (PR #157 review).
const LAYOUT_SEP: &str = "; ";

/// What `part` adds to the width of the joined map: itself, plus a separator only when something
/// already precedes it. `join` writes N-1 separators for N parts, so charging one per part bills the
/// map for bytes it never occupies.
fn joined_cost(part: &str, preceded: bool) -> usize {
    part.len() + if preceded { LAYOUT_SEP.len() } else { 0 }
}

/// A compact, deterministic map of what is at `dir`'s root — the thing no unit prompt carried.
///
/// FINDING-048: 0 of 32 prompts described the target tree, and 12 of 32 sessions burned turns on
/// `cd: no such file or directory` rediscovering that AutoGPT is a two-era monorepo. The worker knows
/// its task and nothing about where the task lives, so it guesses paths and pays for each miss.
///
/// SINGLE-LINE by contract, for the same reason as [`crate::assumptions::PROMPT_CONVENTION`]: the PTY
/// session runner writes a prompt line-based, so an embedded newline would end the turn early and send
/// the rest of the map as its own turn. `;` separates top-level entries, `{…}` holds a descent.
///
/// Deliberately shallow. Depth 1 always; depth 2 ONLY for a top-level directory that is not itself a
/// project root but contains ones — precisely the monorepo shape that produced the failures, and the
/// only case where the extra level carries information a worker cannot get from `ls`. Entries are
/// sorted, so the same tree yields the same string on every host and every run.
///
/// Returns `None` when `dir` cannot be read or has nothing worth reporting, so a caller that has no
/// worktree (or an empty one) appends nothing rather than an empty heading.
#[must_use]
pub(crate) fn worktree_layout(dir: &Path) -> Option<String> {
    worktree_layout_within(dir, LAYOUT_BUDGET)
}

/// [`worktree_layout`] against a caller-supplied character budget.
///
/// The PTY session runner needs this: it writes a prompt as ONE line, and a pty in canonical mode
/// discards any line that reaches `MAX_CANON` (1024 bytes) without ever delivering it, so the map has
/// to fit in whatever the rest of the prompt leaves rather than in a fixed 1200. A budget too small
/// for even one entry yields `None`, which reads as "no map" and costs the caller nothing.
#[must_use]
pub(crate) fn worktree_layout_within(dir: &Path, layout_budget: usize) -> Option<String> {
    let (dirs, files) = read_split(dir)?;
    if dirs.is_empty() && files.is_empty() {
        return None;
    }

    let mut parts: Vec<String> = Vec::new();
    let mut budget = layout_budget;
    let mut truncated = dirs.len() > MAX_TOP_LEVEL;

    for name in dirs.iter().take(MAX_TOP_LEVEL) {
        let child = dir.join(name);
        let mut part = format!("{name}/");
        if let Some(m) = manifest_of(&child) {
            part.push_str(&format!(" [{m}]"));
        }
        // Both, not either. An earlier cut of this treated a manifest as "this is the project, stop
        // descending" — and AutoGPT, the repo the finding is ABOUT, rendered as
        // `autogpt_platform/ [Makefile]` with `backend/` and `frontend/` still invisible, because a
        // container can carry a Makefile that drives the projects underneath it. A directory being a
        // project root and being a container of project roots are independent facts; report both.
        if let Some(inner) = project_children(&child) {
            part.push_str(&format!(" {{{}}}", inner.join(", ")));
        }
        // The separator is only paid for when there is a previous part to join this one to.
        let cost = joined_cost(&part, !parts.is_empty());
        if cost > budget {
            truncated = true;
            break;
        }
        budget -= cost;
        parts.push(part);
    }

    // Root files last and cheaply: they matter far less than the directory shape, and a worker can
    // always `ls`. Listing them at all is what tells it whether the root IS the project.
    if !files.is_empty() {
        if files.len() > MAX_ROOT_FILES {
            truncated = true;
        }
        let joined = files
            .iter()
            .take(MAX_ROOT_FILES)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        let part = format!("root files: {joined}");
        let cost = joined_cost(&part, !parts.is_empty());
        if cost <= budget {
            parts.push(part);
        } else {
            truncated = true;
        }
    }

    if parts.is_empty() {
        return None;
    }
    let mut out = parts.join(LAYOUT_SEP);
    if truncated {
        out.push_str(LAYOUT_TRUNCATED);
    }
    Some(out)
}

/// Split a directory into (subdirectory names, file names), both sorted, both filtered of hidden
/// entries and [`LAYOUT_NOISE`]. `None` if the dir cannot be read at all.
fn read_split(dir: &Path) -> Option<(Vec<String>, Vec<String>)> {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Hidden entries are excluded wholesale — `.git` above all, which is enormous and never the
        // subject of a work unit.
        if name.starts_with('.') || LAYOUT_NOISE.contains(&name.as_str()) {
            continue;
        }
        match entry.file_type() {
            Ok(t) if t.is_dir() => dirs.push(name),
            Ok(_) => files.push(name),
            Err(_) => continue,
        }
    }
    dirs.sort();
    files.sort();
    Some((dirs, files))
}

/// The first [`PROJECT_MANIFESTS`] entry present directly in `dir`, if any. Order is the constant's
/// order, so the answer is stable for a directory carrying more than one.
fn manifest_of(dir: &Path) -> Option<&'static str> {
    PROJECT_MANIFESTS
        .iter()
        .copied()
        .find(|m| dir.join(m).is_file())
}

/// The child directories of `dir` that ARE project roots, rendered `name/ [manifest]`. `None` when
/// there are none — which is what keeps depth 2 from firing on ordinary nested directories.
fn project_children(dir: &Path) -> Option<Vec<String>> {
    let (dirs, _) = read_split(dir)?;
    let found: Vec<String> = dirs
        .iter()
        .filter_map(|name| manifest_of(&dir.join(name)).map(|m| format!("{name}/ [{m}]")))
        .take(MAX_CHILDREN)
        .collect();
    (!found.is_empty()).then_some(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// crew#276 — new worktrees land at the non-dotted root, while a run whose worktree already
    /// exists under the legacy `.wicked/worktrees` keeps resolving there (resume/reap compat).
    #[test]
    fn worktree_path_is_non_dotted_and_prefers_a_legacy_tree_when_present() {
        let repo = std::env::temp_dir().join(format!("wt-move-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(&repo).unwrap();
        let root = repo.to_str().unwrap();

        // Fresh run: the non-dotted root.
        let fresh = worktree_path(root, "run-new");
        assert!(
            fresh.starts_with(repo.join("wicked-worktrees")),
            "a new run's worktree must live under the non-dotted root: {}",
            fresh.display()
        );
        assert!(
            !fresh.to_string_lossy().contains("/.wicked/"),
            "no dot-segment in a new worktree path"
        );

        // Pre-move run: a directory already under the legacy root wins.
        let legacy = repo.join(".wicked").join("worktrees").join("run-old");
        std::fs::create_dir_all(&legacy).unwrap();
        assert_eq!(
            worktree_path(root, "run-old"),
            legacy,
            "an existing legacy worktree must keep resolving for resume/reap"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn slug_takes_four_kebab_words() {
        assert_eq!(slug("My Cool Repo Name Extra"), "my-cool-repo-name");
        assert_eq!(slug("!!!"), "repo");
    }

    /// A scratch tree, named per-test AND per-process so concurrent test binaries never collide.
    /// Each entry is `"a/b/c"` for a directory or `"a/b/file.ext"` for an (empty) file — the layout
    /// only ever looks at names and file-vs-dir, never at content.
    fn scratch(name: &str, entries: &[&str]) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "wicked-layout-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for e in entries {
            let p = root.join(e);
            if p.extension().is_some() {
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(&p, "").unwrap();
            } else {
                std::fs::create_dir_all(&p).unwrap();
            }
        }
        root
    }

    /// The exact shape FINDING-048 was about: `cd autogpt_platform/backend` is not guessable from
    /// depth 1, so depth 1 alone would have left the 12 failing sessions failing. Pinned as a whole
    /// string, which also holds the sort (a map that reorders between runs is not a stable prompt)
    /// and the single-line contract.
    ///
    /// `classic/` is the case that a first cut of this got WRONG and the real AutoGPT clone exposed:
    /// it carries a manifest AND contains project roots. Treating the manifest as "stop here" hid
    /// `autogpt_platform/backend` behind `autogpt_platform/ [Makefile]` — the one path the finding
    /// exists to surface. Both facts are reported.
    #[test]
    fn a_project_root_that_also_contains_projects_reports_both() {
        let root = scratch(
            "monorepo",
            &[
                "autogpt_platform/backend/pyproject.toml",
                "autogpt_platform/frontend/package.json",
                "classic/pyproject.toml",
                "classic/forge/setup.py",
                "docs/content",
                "README.md",
            ],
        );
        let map = worktree_layout(&root).expect("a populated tree has a map");
        assert_eq!(
            map,
            "autogpt_platform/ {backend/ [pyproject.toml], frontend/ [package.json]}; \
             classic/ [pyproject.toml] {forge/ [setup.py]}; docs/; root files: README.md"
        );
        // Stated separately from the equality above because it is a CONTRACT, not an incidental
        // property of this fixture: the PTY runner writes the prompt line-based.
        assert!(!map.contains('\n'), "the map must stay single-line: {map}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `.git` alone would dwarf the rest of the map, and no work unit is ever about `node_modules`.
    #[test]
    fn hidden_and_build_output_directories_never_reach_the_prompt() {
        let root = scratch(
            "noise",
            &[
                ".git/objects",
                ".venv/lib",
                "node_modules/react",
                "target/debug",
                "dist/bundle.js",
                "src/main.rs",
                "Cargo.toml",
            ],
        );
        let map = worktree_layout(&root).unwrap();
        assert_eq!(map, "src/; root files: Cargo.toml");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A budget that exactly fits the map spends all of it and clips nothing.
    ///
    /// The separator is only written BETWEEN entries, so an N-entry map pays for N-1 of them. An
    /// earlier cut charged one per entry (PR #157 review), which made a map cost 2 bytes more than
    /// it occupies — enough to drop the last entry and stamp a complete map `…truncated` a byte
    /// before it had to. Harmless-looking, but the pty path is precisely where the budget is small
    /// and computed, so 2 bytes is a whole root-files line there.
    ///
    /// Pinned at the exact boundary in both directions: one byte less genuinely does not fit.
    #[test]
    fn a_budget_that_exactly_fits_the_map_clips_nothing() {
        let root = scratch("exact", &["src/main.rs", "Cargo.toml"]);
        let whole = "src/; root files: Cargo.toml";
        // `whole.len()` rather than a literal: the point is that the budget equals the OUTPUT, and
        // hardcoding it would restate the same off-by-two this test exists to catch.
        let map = worktree_layout_within(&root, whole.len()).expect("an exactly-fitting map");
        assert_eq!(map, whole, "a budget equal to the map must not clip it");

        let tight = worktree_layout_within(&root, whole.len() - 1).unwrap();
        assert!(
            tight.ends_with(LAYOUT_TRUNCATED) && !tight.contains("Cargo.toml"),
            "one byte short must actually clip, or the budget means nothing: {tight}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Bounded, and HONEST about being bounded — a clipped map that reads as complete would send a
    /// worker looking for a directory it was simply never shown.
    #[test]
    fn a_wide_tree_is_clipped_and_says_so() {
        let mut entries: Vec<String> = (0..MAX_TOP_LEVEL + 5)
            .map(|i| format!("dir{i:03}"))
            .collect();
        entries.extend((0..MAX_ROOT_FILES + 5).map(|i| format!("file{i:03}.txt")));
        let refs: Vec<&str> = entries.iter().map(String::as_str).collect();
        let root = scratch("wide", &refs);
        let map = worktree_layout(&root).unwrap();
        assert!(
            map.ends_with(LAYOUT_TRUNCATED),
            "a clipped map must say so: {map}"
        );
        assert!(
            map.len() <= LAYOUT_BUDGET + LAYOUT_TRUNCATED.len(),
            "the map must stay inside its budget, got {} chars",
            map.len()
        );
        assert!(
            map.contains(&format!("dir{:03}/", MAX_TOP_LEVEL - 1))
                && !map.contains(&format!("dir{MAX_TOP_LEVEL:03}/")),
            "the directory cap is where it says it is: {map}"
        );
        assert!(
            map.contains(&format!("file{:03}.txt", MAX_ROOT_FILES - 1))
                && !map.contains(&format!("file{MAX_ROOT_FILES:03}.txt")),
            "root files get their own tighter cap: {map}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// No worktree, or an empty one, must append NOTHING — a bare heading over an empty map is worse
    /// than silence, because it reads as "this repo has nothing in it".
    #[test]
    fn nothing_worth_saying_yields_no_map_at_all() {
        assert_eq!(
            worktree_layout(&std::env::temp_dir().join("wicked-layout-does-not-exist")),
            None
        );
        let root = scratch("empty", &[]);
        assert_eq!(worktree_layout(&root), None);
        let hidden_only = scratch("hidden-only", &[".git/objects"]);
        assert_eq!(worktree_layout(&hidden_only), None);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&hidden_only);
    }

    #[test]
    fn repo_entry_round_trips_through_node() {
        // Read side of the estate-home env lock: `code_graph_db` resolves through
        // `WICKED_ESTATE_REPO_GRAPH_ROOT`, and this test resolves TWICE (build + from_node) —
        // an env-mutating test flipping the root between the two would make this flake.
        let _env = crate::code_graph::REPO_GRAPH_ROOT_ENV_LOCK
            .read()
            .unwrap_or_else(|p| p.into_inner());
        let e = RepoEntry {
            id: "demo".into(),
            name: "Demo".into(),
            root_path: "/tmp/demo".into(),
            default_branch: "main".into(),
            registered_at: 42,
            code_graph_db: code_graph_db("/tmp/demo"),
        };
        assert_eq!(RepoEntry::from_node(&e.to_node()).unwrap(), e);
    }

    /// A record written before `code_graph_db` existed must read back with the path filled in, not
    /// with an empty string that a consumer would join onto or hand to a store opener.
    ///
    /// This is the arm that makes the field safe to add without a migration: `from_node` derives it
    /// from `root_path` and ignores whatever the metadata said, so every record ever persisted — and
    /// every record persisted by a future version that gets the derivation wrong — reads correct.
    #[test]
    fn a_record_predating_the_field_still_resolves_its_code_graph() {
        // Read side of the estate-home env lock — double resolution, same as the round-trip test.
        let _env = crate::code_graph::REPO_GRAPH_ROOT_ENV_LOCK
            .read()
            .unwrap_or_else(|p| p.into_inner());
        let mut node = RepoEntry {
            id: "legacy".into(),
            name: "Legacy".into(),
            root_path: "/tmp/legacy".into(),
            default_branch: "main".into(),
            registered_at: 7,
            code_graph_db: String::new(),
        }
        .to_node();
        node.metadata.remove("code_graph_db");
        // Also covers the stale case: a persisted value from before the repo moved.
        let mut moved = node.clone();
        moved.metadata.insert(
            "code_graph_db".into(),
            serde_json::Value::String("/somewhere/else/old.db".into()),
        );

        for n in [node, moved] {
            let back = RepoEntry::from_node(&n).unwrap();
            assert_eq!(back.code_graph_db, code_graph_db("/tmp/legacy"));
        }
    }

    /// The record publishes the RESOLVER's answer, in both homes: a repo with an in-tree graph
    /// publishes exactly that file (continuity — an indexed repo never migrates silently, AC2);
    /// one without publishes its estate-home path (AC1), and its working tree stays clean. The
    /// consumer-literal pins (including the Windows segment-join trap) live with the resolver, in
    /// `code_graph::tests::the_spellings_are_the_ones_consumers_expect`.
    #[test]
    fn the_record_publishes_the_resolver_answer_for_both_homes() {
        let _env = crate::code_graph::REPO_GRAPH_ROOT_ENV_LOCK
            .read()
            .unwrap_or_else(|p| p.into_inner());
        // In-tree graph on disk → published verbatim.
        let root = std::env::temp_dir().join(format!(
            "wc-spell-legacy-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let legacy = root.join(crate::code_graph::code_graph_rel());
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, b"indexed").unwrap();
        assert_eq!(
            code_graph_db(root.to_str().unwrap()),
            legacy.to_string_lossy(),
            "an already-indexed repo keeps publishing its in-tree graph"
        );
        let _ = std::fs::remove_dir_all(&root);

        // No in-tree graph → the estate home (when one resolves; a host with no home at all is
        // the documented in-tree fallback).
        let bare = std::env::temp_dir().join(format!(
            "wc-spell-bare-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&bare);
        std::fs::create_dir_all(&bare).unwrap();
        let got = code_graph_db(bare.to_str().unwrap());
        match crate::code_graph::repo_graph_root() {
            Some(estate_root) => assert_eq!(
                got,
                crate::code_graph::estate_home_graph_db_at(&estate_root, &bare).to_string_lossy(),
                "a fresh repo publishes its estate-home path"
            ),
            None => assert_eq!(
                got,
                bare.join(crate::code_graph::code_graph_rel())
                    .to_string_lossy()
            ),
        }
        assert!(
            !bare
                .join(crate::code_graph::code_graph_rel())
                .parent()
                .unwrap()
                .exists(),
            "deriving the record path must not pollute the working tree"
        );
        let _ = std::fs::remove_dir_all(&bare);
    }

    // ── worktree isolation (FINDING-059) ──────────────────────────────────────
    //
    // The defect these pin was one stat: `create_worktree` returned any directory sitting at the
    // worktree path as an isolated checkout. The state that exploited it — an empty, unregistered
    // directory left by a partial `remove_worktree` — is cheap to construct, so it is constructed
    // here rather than described.

    /// A git repo with one commit at a scratch path. Identity and signing are set locally because
    /// `commit` fails without the first and can hang on the second, and neither is what these are
    /// about. Named per-process AND per-thread so concurrent test binaries never collide.
    fn git_repo(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "wicked-wt-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let p = root.to_str().unwrap();
        for args in [
            &["init", "-q"][..],
            &["config", "user.email", "t@example.invalid"],
            &["config", "user.name", "wicked-test"],
            &["config", "commit.gpgsign", "false"],
        ] {
            assert!(git(p, args).unwrap().0, "git {args:?} failed");
        }
        std::fs::write(root.join("README.md"), "base\n").unwrap();
        assert!(git(p, &["add", "-A"]).unwrap().0);
        assert!(git(p, &["commit", "-qm", "base"]).unwrap().0);
        root
    }

    /// FINDING-009. Coverage for a repo must be computed over the REPO's own code graph, not the
    /// daemon store. We register a repo whose `code_graph_db` holds 3 nodes and a daemon store that
    /// holds only the RepoEntry (1 node): `report.total` (all-kinds node count) must be 3, proving the
    /// repo store was read. Mutation — recomputing over `daemon` instead — yields the daemon's count
    /// (not 3), failing here. An unknown repo_ref errors rather than returning a vacuous report.
    #[test]
    fn coverage_report_for_repo_reads_the_repo_store_not_the_daemon() {
        let root = git_repo("cov009");
        // Pin the repo to a LEGACY in-tree graph (touch the file first, so the resolver
        // short-circuits): hermetic — nothing lands in a real estate home — and env-immune, so a
        // concurrently-running `WICKED_ESTATE_REPO_GRAPH_ROOT` test cannot move the path between
        // this resolution and the ones inside register_repo/coverage_report_for_repo.
        let legacy = root.join(crate::code_graph::code_graph_rel());
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, b"").unwrap();
        // The repo's OWN code graph: 3 arbitrary nodes (total counts all kinds, so plain nodes work).
        let cg_path = code_graph_db(root.to_str().unwrap());
        assert_eq!(cg_path, legacy.to_string_lossy(), "precondition: in-tree");
        {
            let mut repo_store = wicked_apps_core::open_store(Some(&cg_path)).unwrap();
            for i in 0..3 {
                let n = Node::new(
                    synthetic_symbol("thing", &format!("n{i}")),
                    NodeKind::Other("thing".to_string()),
                    format!("n{i}"),
                    Language::new(SYMBOL_SCHEME),
                    Location::new(format!("thing/n{i}"), Span::ZERO),
                );
                put_node(&mut repo_store, n).unwrap();
            }
        }
        // The daemon store: a DIFFERENT file that ends up with just the RepoEntry node (count != 3).
        let daemon_path = root.join("daemon-store.db");
        let mut daemon = wicked_apps_core::open_store(Some(daemon_path.to_str().unwrap())).unwrap();
        let entry = register_repo(
            &mut daemon,
            RepoSpec {
                name: "Cov 009".into(),
                root_path: root.to_str().unwrap().into(),
                registered_at: 0,
            },
        )
        .unwrap();

        let report = coverage_report_for_repo(&daemon, &entry.id).unwrap();
        assert_eq!(
            report.total, 3,
            "coverage is computed over the REPO store (3 nodes), not the daemon store"
        );

        // An unknown repo is an error, never a vacuous 1.0 report.
        assert!(
            coverage_report_for_repo(&daemon, "no-such-repo").is_err(),
            "an unknown repo_ref must error, not return a vacuous report"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// #122 web surface: `graph_kinds_for_repo` summarises a repo's OWN graph (node counts by kind),
    /// read over the repo store — NOT the daemon. Mutation — resolving against `daemon` — yields the
    /// daemon's single RepoEntry node instead of the seeded function/struct counts, failing here. An
    /// unknown repo errors rather than returning an empty summary.
    #[test]
    fn graph_kinds_for_repo_summarises_the_repo_store_not_the_daemon() {
        let root = git_repo("kinds122");
        // Legacy in-tree pin, for the same hermeticity/env-immunity reasons as the coverage test.
        let legacy = root.join(crate::code_graph::code_graph_rel());
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, b"").unwrap();
        let cg_path = code_graph_db(root.to_str().unwrap());
        assert_eq!(cg_path, legacy.to_string_lossy(), "precondition: in-tree");
        {
            let mut repo_store = wicked_apps_core::open_store(Some(&cg_path)).unwrap();
            // Two functions + one struct, so the kind histogram is distinguishable from any count.
            for (kind, i) in [("function", 0), ("function", 1), ("struct", 0)] {
                let n = Node::new(
                    synthetic_symbol(kind, &format!("{kind}{i}")),
                    NodeKind::Other(kind.to_string()),
                    format!("{kind}{i}"),
                    Language::new(SYMBOL_SCHEME),
                    Location::new(format!("{kind}/{kind}{i}"), Span::ZERO),
                );
                put_node(&mut repo_store, n).unwrap();
            }
        }
        let daemon_path = root.join("daemon-store.db");
        let mut daemon = wicked_apps_core::open_store(Some(daemon_path.to_str().unwrap())).unwrap();
        let entry = register_repo(
            &mut daemon,
            RepoSpec {
                name: "Kinds 122".into(),
                root_path: root.to_str().unwrap().into(),
                registered_at: 0,
            },
        )
        .unwrap();

        let kinds = graph_kinds_for_repo(&daemon, &entry.id).unwrap();
        // BTreeMap-ordered: "function" (2) before "struct" (1) — the REPO's histogram, not the daemon's.
        assert_eq!(
            kinds,
            vec![("function".to_string(), 2), ("struct".to_string(), 1)],
            "graph kinds must summarise the REPO store, not the daemon"
        );

        assert!(
            graph_kinds_for_repo(&daemon, "no-such-repo").is_err(),
            "an unknown repo_ref must error, not return an empty summary"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// core#214. `register_repo` must persist an ABSOLUTE, normalised root — never the caller's
    /// as-given spelling. A relative `--path ./repo` stored verbatim resolves to nothing from the
    /// daemon's cwd, and the `code_graph_db` derived from it inherits the break.
    ///
    /// The input carries a redundant `.` (CurDir) component: `std::path::absolute` strips it on EVERY
    /// platform, so the stored root differs from the as-given spelling — reverting to store-as-given
    /// fails `assert_eq`. (`..` is deliberately NOT used: `absolute` keeps `..` on POSIX to preserve
    /// symlink meaning, so it would not falsify there.) We assert equality to `absolute(root)`, NOT to
    /// `canonicalize`: the fix must not resolve symlinks (canonicalize breaks `git worktree add` on
    /// Windows via `\?\` and rewrites `/var`→`/private/var` on macOS — see the fn's doc comment).
    #[test]
    fn register_repo_stores_an_absolute_normalised_root() {
        // Read side of the estate-home env lock: a fresh repo's code_graph_db resolves through
        // `WICKED_ESTATE_REPO_GRAPH_ROOT`, and this test resolves more than once.
        let _env = crate::code_graph::REPO_GRAPH_ROOT_ENV_LOCK
            .read()
            .unwrap_or_else(|p| p.into_inner());
        let root = git_repo("core214");
        let messy = root.join("."); // `<root>/.` — absolute but not normalised
        let expected = std::path::absolute(&root)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_ne!(
            messy.to_string_lossy(),
            expected,
            "precondition: the registered spelling is not already normalised"
        );

        let mut store = wicked_apps_core::open_store(Some(":memory:")).unwrap();
        let entry = register_repo(
            &mut store,
            RepoSpec {
                name: "Core 214 Repo".into(),
                root_path: messy.to_string_lossy().into_owned(),
                registered_at: 0,
            },
        )
        .unwrap();

        assert!(
            Path::new(&entry.root_path).is_absolute(),
            "root_path must be absolute: {}",
            entry.root_path
        );
        assert_eq!(
            entry.root_path, expected,
            "root_path is stored absolute + normalised, not as-given"
        );
        assert!(
            Path::new(&entry.code_graph_db).is_absolute(),
            "code_graph_db is absolute: {}",
            entry.code_graph_db
        );
        assert_eq!(
            entry.code_graph_db,
            crate::code_graph::resolved_code_graph_db(Path::new(&expected)).to_string_lossy(),
            "code_graph_db is the resolver's answer for the NORMALISED root (a fresh repo's \
             graph lives in the estate home, not the working tree)"
        );

        // The persisted node round-trips to the SAME paths (FromNode re-derives code_graph_db from
        // root_path), so a consumer reading the store back never sees the as-given spelling.
        let fetched = get_repo(&store, &entry.id).unwrap().unwrap();
        assert_eq!(fetched.root_path, expected);
        assert_eq!(fetched.code_graph_db, entry.code_graph_db);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// AC2 at the registration seam: a repo that ALREADY has an in-tree graph registers with that
    /// exact path — read and write both keep it (the resolver is legacy-first; FINDING-069's
    /// never-orphan lesson). Zero behavior change for every repo indexed before the estate home.
    #[test]
    fn registration_keeps_an_existing_in_tree_graph_for_read_and_write() {
        let root = git_repo("legacy-keep");
        let legacy = root.join(crate::code_graph::code_graph_rel());
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, b"indexed before the estate home existed").unwrap();

        let mut store = wicked_apps_core::open_store(Some(":memory:")).unwrap();
        let entry = register_repo(
            &mut store,
            RepoSpec {
                name: "Legacy Keep".into(),
                root_path: root.to_string_lossy().into_owned(),
                registered_at: 0,
            },
        )
        .unwrap();
        let expected = std::path::absolute(&root)
            .unwrap()
            .join(crate::code_graph::code_graph_rel());
        assert_eq!(
            entry.code_graph_db,
            expected.to_string_lossy(),
            "the record keeps publishing the in-tree graph"
        );
        assert_eq!(
            crate::code_graph::code_graph_path_for_write(&root).unwrap(),
            legacy,
            "the WRITE path (re-index) also keeps the in-tree graph — never a silent fork"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// AC1 at the registration seam: a repo with NO in-tree graph publishes its estate-home path
    /// on the record, and registration leaves the working tree clean. Runs under the env override
    /// (write lock) so nothing resolves against — or writes into — a real home.
    #[test]
    fn registration_with_no_in_tree_graph_publishes_the_estate_home() {
        let _env = crate::code_graph::REPO_GRAPH_ROOT_ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var_os(crate::code_graph::REPO_GRAPH_ROOT_ENV);
        let estate_root = std::env::temp_dir().join(format!(
            "wc-reg-estate-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::env::set_var(crate::code_graph::REPO_GRAPH_ROOT_ENV, &estate_root);

        let root = git_repo("estate-reg");
        let mut store = wicked_apps_core::open_store(Some(":memory:")).unwrap();
        let entry = register_repo(
            &mut store,
            RepoSpec {
                name: "Estate Reg".into(),
                root_path: root.to_string_lossy().into_owned(),
                registered_at: 0,
            },
        )
        .unwrap();

        assert_eq!(
            entry.code_graph_db,
            crate::code_graph::estate_home_graph_db_at(&estate_root, &root).to_string_lossy(),
            "the record publishes the estate-home path"
        );
        assert!(
            !root
                .join(crate::code_graph::code_graph_rel())
                .parent()
                .unwrap()
                .exists(),
            "registration must not pollute the working tree"
        );
        // And the persisted node round-trips to the same answer (FromNode re-derives).
        let fetched = get_repo(&store, &entry.id).unwrap().unwrap();
        assert_eq!(fetched.code_graph_db, entry.code_graph_db);

        match prev {
            Some(v) => std::env::set_var(crate::code_graph::REPO_GRAPH_ROOT_ENV, v),
            None => std::env::remove_var(crate::code_graph::REPO_GRAPH_ROOT_ENV),
        }
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&estate_root);
    }

    /// The heart of FINDING-059. `rev-parse` succeeds anywhere beneath the parent repo, including in
    /// an empty `.wicked/worktrees/<id>/` — so a check that trusts it (or trusts `is_dir`, as the
    /// original did) calls the parent repo's own working tree an isolated checkout.
    #[test]
    fn an_empty_dir_under_the_repo_is_not_a_worktree_though_rev_parse_succeeds_in_it() {
        let root = git_repo("revparse");
        let empty = worktrees_root(root.to_str().unwrap()).join("run-1");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(
            git(empty.to_str().unwrap(), &["rev-parse", "--git-dir"])
                .unwrap()
                .0,
            "precondition: rev-parse resolves here, which is the trap"
        );
        assert!(!is_live_worktree(&empty));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn create_worktree_reuses_a_live_worktree() {
        let root = git_repo("reuse");
        let p = root.to_str().unwrap();
        let first = create_worktree(p, "run-1").unwrap();
        std::fs::write(first.join("worker-output.txt"), "from turn 1\n").unwrap();

        let second = create_worktree(p, "run-1").unwrap();
        assert_eq!(first, second);
        // Reuse has to mean the same checkout, not a re-add that discards the last turn's work.
        assert_eq!(
            std::fs::read_to_string(second.join("worker-output.txt")).unwrap(),
            "from turn 1\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The observed state: `remove_worktree`'s `remove_dir_all` fallback stripped the `.git` file
    /// but left the directory, and the `prune` that follows deregistered the path *because* `.git`
    /// was gone. Recovery, not reuse — the shell is not a checkout.
    #[test]
    fn create_worktree_recovers_an_empty_shell_left_by_a_partial_removal() {
        let root = git_repo("shell");
        let p = root.to_str().unwrap();
        let wt = create_worktree(p, "run-1").unwrap();
        std::fs::remove_dir_all(&wt).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        assert!(git(p, &["worktree", "prune"]).unwrap().0);
        assert!(!is_live_worktree(&wt), "precondition: shell, not worktree");

        let again = create_worktree(p, "run-1").unwrap();
        assert_eq!(again, wt);
        assert!(
            is_live_worktree(&again),
            "the run must get a real checkout, not the shell it was handed before"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Same shell, prune never ran, so git may still hold an admin entry for the path — and
    /// `worktree add` refuses a path it considers registered. The prune inside `create_worktree`
    /// is what keeps this recoverable.
    #[test]
    fn create_worktree_recovers_a_shell_whose_registration_was_never_pruned() {
        let root = git_repo("stale-reg");
        let p = root.to_str().unwrap();
        let wt = create_worktree(p, "run-1").unwrap();
        std::fs::remove_dir_all(&wt).unwrap();
        std::fs::create_dir_all(&wt).unwrap();

        let again = create_worktree(p, "run-1").unwrap();
        assert!(
            is_live_worktree(&again),
            "worktrees still registered: {}",
            git(p, &["worktree", "list"]).unwrap().1
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The case that must fail the run rather than proceed: a directory of unknown provenance. It
    /// cannot be cleared (that would delete someone's work) and it cannot be handed over (that is
    /// the 297 lines on `master`), so the only honest answer is to stop.
    #[test]
    fn create_worktree_refuses_a_non_empty_directory_that_is_not_a_worktree() {
        let root = git_repo("occupied");
        let p = root.to_str().unwrap();
        let wt = worktrees_root(p).join("run-1");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join("someone-elses-work.txt"), "not ours\n").unwrap();

        let err = create_worktree(p, "run-1").unwrap_err().to_string();
        assert!(err.contains("is not a git worktree"), "{err}");
        // A refusal must not double as a delete.
        assert!(wt.join("someone-elses-work.txt").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// FINDING-003, the common case: a terminal run whose tree is clean loses the CHECKOUT but
    /// keeps the BRANCH — the branch is the durable record an operator reviews/merges; the
    /// worktree was scaffolding.
    #[test]
    fn reap_if_clean_removes_a_clean_worktree_but_never_its_branch() {
        let root = git_repo("reap-clean");
        let p = root.to_str().unwrap();
        let wt = create_worktree(p, "run-1").unwrap();
        assert!(is_live_worktree(&wt), "precondition: a real checkout");

        assert!(reap_worktree_if_clean(p, "run-1"), "a clean tree reaps");
        assert!(!wt.exists(), "the checkout is gone");
        let (ok, branches, _) = git(p, &["branch", "--list", "wicked/run-1"]).unwrap();
        assert!(
            ok && branches.contains("wicked/run-1"),
            "the wicked/run-1 branch must survive the reap — it is the record, got: {branches}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// FINDING-003, the safety property the whole design leans on: a tree holding uncommitted
    /// work (3 of the finding's 14 orphans did — the artifact-landing gap) is KEPT, bytes intact.
    /// If this fails, the reaper has become the thing that destroys the only copy of the work.
    #[test]
    fn reap_if_clean_keeps_a_dirty_worktree_and_its_unlanded_bytes() {
        let root = git_repo("reap-dirty");
        let p = root.to_str().unwrap();
        let wt = create_worktree(p, "run-1").unwrap();
        std::fs::write(wt.join("unlanded-artifact.txt"), "never committed\n").unwrap();

        assert!(
            !reap_worktree_if_clean(p, "run-1"),
            "a dirty tree must be reported KEPT"
        );
        assert!(
            is_live_worktree(&wt),
            "the dirty tree stays a live checkout, not a half-removed shell"
        );
        assert_eq!(
            std::fs::read_to_string(wt.join("unlanded-artifact.txt")).unwrap(),
            "never committed\n",
            "the uncommitted bytes are untouched"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// FINDING-003's restart half: the startup reaper must converge terminal runs' leftovers
    /// (14 survived restarts) while never touching a live run's checkout, and still force-clear
    /// worktrees whose run id the store has never heard of.
    #[test]
    fn startup_reaper_reaps_terminal_keeps_live_and_forces_unknown() {
        let root = git_repo("reap-startup");
        let p = root.to_str().unwrap();
        let wt_live = create_worktree(p, "run-live").unwrap();
        let wt_done = create_worktree(p, "run-done").unwrap();
        let wt_dirty = create_worktree(p, "run-dirty").unwrap();
        std::fs::write(wt_dirty.join("unlanded.txt"), "keep me\n").unwrap();
        let wt_gone = create_worktree(p, "run-unknown").unwrap();
        // The unknown-id worktree is dirty TOO — force removal is exactly the point there.
        std::fs::write(wt_gone.join("scratch.txt"), "no session owns this\n").unwrap();

        let repo = RepoEntry {
            id: "r".into(),
            name: "r".into(),
            root_path: p.to_string(),
            default_branch: "main".into(),
            registered_at: 0,
            code_graph_db: String::new(),
        };
        let live: HashSet<String> = ["run-live".to_string()].into_iter().collect();
        let terminal: HashSet<String> = ["run-done".to_string(), "run-dirty".to_string()]
            .into_iter()
            .collect();
        reap_orphan_worktrees(std::slice::from_ref(&repo), &live, &terminal);

        assert!(
            is_live_worktree(&wt_live),
            "a non-terminal run keeps its checkout across restarts (resume)"
        );
        assert!(!wt_done.exists(), "a clean terminal leftover converges");
        assert!(
            is_live_worktree(&wt_dirty) && wt_dirty.join("unlanded.txt").is_file(),
            "a dirty terminal leftover is kept — same rule as the terminal-status reap"
        );
        assert!(
            !wt_gone.exists(),
            "a worktree no session owns is force-removed, dirty or not"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── worktree name sanitization (core#337) ─────────────────────────────────
    //
    // A campaign node's run id is `{campaign}:{node}:a{attempt}`, and `:` is illegal in a git ref
    // and in an NTFS file name — `create_worktree` naming branches `wicked/{run_id}` VERBATIM
    // failed every repo-scoped campaign node at dispatch with "is not a valid branch name".

    /// Plain run ids (uuids) must pass through byte-for-byte: their worktrees and branches are
    /// already on operators' disks under the raw spelling, and any change strands them.
    #[test]
    fn sanitize_leaves_a_plain_id_untouched() {
        for id in ["8b9b1a2c-3d4e-5f60-8192-a3b4c5d6e7f8", "run-1", "a.b_c"] {
            assert_eq!(sanitize_worktree_id(id), id);
        }
    }

    /// The campaign shape maps `':' → '-'` with NO hash suffix — deliberately the convention
    /// crew's shipped workaround (crew#390/#391) already stamps on operators' repos
    /// (`wicked/<id with ':' → '-'>`), so engine and daemon spell the SAME branch.
    #[test]
    fn sanitize_maps_a_campaign_id_to_crews_shipped_convention() {
        assert_eq!(sanitize_worktree_id("camp:node:a0"), "camp-node-a0");
        assert_eq!(
            sanitize_worktree_id("web-refresh:lint:a12"),
            "web-refresh-lint-a12"
        );
    }

    /// The residual tier (illegal content beyond `:`) is pinned EXACTLY, hash and all. The
    /// spelling is an on-disk directory name and a git branch that resume/reap re-derive after an
    /// engine upgrade — if this pin moves, existing worktrees strand.
    #[test]
    fn sanitize_pins_the_residual_mapping_and_its_hash() {
        assert_eq!(sanitize_worktree_id("a?b"), "a-b-e657a319");
        assert_eq!(sanitize_worktree_id("a..b"), "a.-b-6b848b82");
        assert_eq!(sanitize_worktree_id("x.lock"), "x-lock-88586aac");
        assert_eq!(sanitize_worktree_id("nul"), "nul-2102ba19");
        assert_eq!(sanitize_worktree_id("@"), "@-af63fd4c");
        assert_eq!(sanitize_worktree_id("trailing."), "trailing--fe043e6e");
    }

    /// Collision-awareness: two distinct ids that flatten to the same chars must NOT silently
    /// share a worktree — the hash suffix of the RAW id keeps them apart.
    #[test]
    fn sanitize_disambiguates_ids_that_flatten_identically() {
        let a = sanitize_worktree_id("a?b");
        let b = sanitize_worktree_id("a*b");
        assert!(a.starts_with("a-b-") && b.starts_with("a-b-"));
        assert_ne!(a, b, "identical flattening must not mean a shared worktree");
    }

    /// Every Windows/NTFS-illegal character is mapped, not just `:` — the worktree NAME is a
    /// directory, and a `:` (or `?`, `|`, ...) in it keeps campaign nodes broken on win32 even
    /// with a legal branch. Also the NTFS oddities: trailing dot, reserved device names.
    #[test]
    fn sanitize_covers_the_windows_illegal_set() {
        for c in [
            '<', '>', ':', '"', '|', '?', '*', '\\', '/', '\u{1}', '\u{1f}',
        ] {
            let got = sanitize_worktree_id(&format!("x{c}y"));
            assert!(!got.contains(c), "{c:?} must be mapped, got {got}");
            assert!(got.starts_with("x-y"), "{c:?} maps to '-', got {got}");
        }
        assert!(!sanitize_worktree_id("trailing.").ends_with('.'));
        assert_ne!(sanitize_worktree_id("nul"), "nul");
        assert_ne!(sanitize_worktree_id("COM1"), "COM1");
        // Not reserved — must stay byte-for-byte.
        assert_eq!(sanitize_worktree_id("console"), "console");
        assert_eq!(sanitize_worktree_id("COM0"), "COM0");
    }

    /// git's ref rules beyond single chars (`check-ref-format`): no `..`, no `@{`, no leading or
    /// trailing `.`, no `.lock` suffix, not a lone `@`. And the output must be its own fixed
    /// point — the startup reaper re-derives ids from directory NAMES, which may already be
    /// sanitized spellings.
    #[test]
    fn sanitize_output_is_a_valid_ref_component_and_a_fixed_point() {
        for bad in [
            "a..b",
            "a@{b",
            ".lead",
            "trail.",
            "x.lock",
            "@",
            "camp:node:a0",
            "a?b",
            "",
            "a b",
        ] {
            let got = sanitize_worktree_id(bad);
            assert!(!got.contains(".."), "{bad:?} → {got}");
            assert!(!got.contains("@{"), "{bad:?} → {got}");
            assert!(!got.starts_with('.'), "{bad:?} → {got}");
            assert!(!got.ends_with('.'), "{bad:?} → {got}");
            assert!(!got.ends_with(".lock"), "{bad:?} → {got}");
            assert_ne!(got, "@");
            assert!(!got.is_empty());
            assert_eq!(
                sanitize_worktree_id(&got),
                got,
                "not idempotent for {bad:?}"
            );
        }
    }

    /// The core#337 repro: a campaign-shaped run id gets a real worktree and a real branch —
    /// `git worktree add -b wicked/camp:node:a0` used to refuse at dispatch. The second call is
    /// the resume contract on the SANITIZED path: same checkout, work preserved.
    #[test]
    fn create_worktree_sanitizes_campaign_shaped_run_ids() {
        let root = git_repo("campaign");
        let p = root.to_str().unwrap();
        let wt = create_worktree(p, "camp:node:a0").unwrap();
        assert!(is_live_worktree(&wt));
        assert_eq!(
            wt.file_name().unwrap().to_str().unwrap(),
            "camp-node-a0",
            "the DIRECTORY carries the sanitized spelling too — ':' is NTFS-illegal"
        );
        let (ok, branches, _) = git(p, &["branch", "--list", "wicked/camp-node-a0"]).unwrap();
        assert!(
            ok && branches.contains("wicked/camp-node-a0"),
            "branch under crew's shipped convention, got: {branches}"
        );
        std::fs::write(wt.join("turn-1.txt"), "landed\n").unwrap();
        let again = create_worktree(p, "camp:node:a0").unwrap();
        assert_eq!(wt, again, "resume re-derives the same sanitized path");
        assert_eq!(
            std::fs::read_to_string(again.join("turn-1.txt")).unwrap(),
            "landed\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// crew#390/#391 COMPAT: crew's shipped workaround pre-provisions a campaign node's worktree
    /// at the engine's layout but the RAW id spelling (`wicked-worktrees/<id with ':'>`), on the
    /// branch-safe branch, riding the documented "a live worktree already at the path is reused"
    /// contract. A fixed engine must FIND that tree — deriving only the sanitized path would try
    /// to mint a second worktree and fail on the branch crew already holds checked out.
    #[cfg(unix)]
    #[test]
    fn create_worktree_reuses_a_crew_preprovisioned_raw_path_worktree() {
        let root = git_repo("crew-prov");
        let p = root.to_str().unwrap();
        std::fs::create_dir_all(worktrees_root(p)).unwrap();
        let raw = worktrees_root(p).join("camp:node:a0");
        // Exactly crew's provisioning: RAW path, branch-SAFE branch.
        let (ok, _, err) = git(
            p,
            &[
                "worktree",
                "add",
                raw.to_str().unwrap(),
                "-b",
                "wicked/camp-node-a0",
            ],
        )
        .unwrap();
        assert!(ok, "precondition — crew's own provisioning works: {err}");
        std::fs::write(raw.join("provisioned.txt"), "crew\n").unwrap();

        let wt = create_worktree(p, "camp:node:a0").unwrap();
        assert_eq!(wt, raw, "the pre-provisioned tree is reused, not shadowed");
        assert_eq!(
            std::fs::read_to_string(wt.join("provisioned.txt")).unwrap(),
            "crew\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The startup reaper matches directory names against the store's RAW run ids — a live
    /// campaign run's checkout sits under its SANITIZED name, and treating it as "unknown to the
    /// store" would force-remove a live run's worktree at every boot.
    #[test]
    fn startup_reaper_recognizes_campaign_worktrees_by_their_sanitized_names() {
        let root = git_repo("reap-campaign");
        let p = root.to_str().unwrap();
        let wt_live = create_worktree(p, "camp:live:a0").unwrap();
        let wt_done = create_worktree(p, "camp:done:a0").unwrap();

        let repo = RepoEntry {
            id: "r".into(),
            name: "r".into(),
            root_path: p.to_string(),
            default_branch: "main".into(),
            registered_at: 0,
            code_graph_db: String::new(),
        };
        // The store holds RAW ids — matching happens across spellings inside the reaper.
        let live: HashSet<String> = ["camp:live:a0".to_string()].into_iter().collect();
        let terminal: HashSet<String> = ["camp:done:a0".to_string()].into_iter().collect();
        reap_orphan_worktrees(std::slice::from_ref(&repo), &live, &terminal);

        assert!(
            is_live_worktree(&wt_live),
            "a live campaign run keeps its checkout — sanitized name must not read as unknown"
        );
        assert!(
            !wt_done.exists(),
            "a clean terminal campaign tree converges"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ADVERSARIAL PROBE (core#337): the colon tier is deliberately hashless (crew#390/#391
    /// branch parity), so two DISTINCT campaign run ids can sanitize IDENTICALLY —
    /// `a-b:c:a0` and `a:b-c:a0` both spell `a-b-c-a0`. The second dispatch must NOT silently
    /// share the first run's live working tree: it fails loudly, and the owner's tree (work
    /// included) survives untouched.
    #[test]
    fn create_worktree_refuses_a_colliding_campaign_id() {
        let root = git_repo("collide");
        let p = root.to_str().unwrap();
        assert_eq!(
            sanitize_worktree_id("a-b:c:a0"),
            sanitize_worktree_id("a:b-c:a0"),
            "precondition — the two ids flatten identically"
        );
        let wt_x = create_worktree(p, "a-b:c:a0").unwrap();
        std::fs::write(wt_x.join("owner-work.txt"), "X\n").unwrap();

        let res = create_worktree(p, "a:b-c:a0");
        assert!(
            res.is_err(),
            "a colliding id must be refused, not handed the owner's tree: {res:?}"
        );
        let msg = format!("{:#}", res.unwrap_err());
        assert!(
            msg.contains("a-b:c:a0"),
            "the refusal names the owning run: {msg}"
        );
        assert!(
            is_live_worktree(&wt_x)
                && std::fs::read_to_string(wt_x.join("owner-work.txt")).unwrap() == "X\n",
            "the owner's tree and work survive the refusal"
        );
        // The owner itself still resumes.
        assert_eq!(create_worktree(p, "a-b:c:a0").unwrap(), wt_x);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ADVERSARIAL PROBE (core#337): the colliding run's own CLEANUP is the other half —
    /// `cancel_run` force-removes and `reap_terminal_worktree` reaps by DERIVING the path from
    /// the run id (actor.rs keys them on `repo_ref` alone), and a colliding id derives the
    /// OWNER's path. Neither destructive form may touch a tree that was minted for a different
    /// raw run id.
    #[test]
    fn cleanup_keyed_by_a_colliding_id_never_touches_the_owners_tree() {
        let root = git_repo("collide-reap");
        let p = root.to_str().unwrap();
        let wt_x = create_worktree(p, "a-b:c:a0").unwrap();
        // X's tree is CLEAN — exactly the state a non-forced reap would happily remove.

        assert!(
            reap_worktree_if_clean(p, "a:b-c:a0"),
            "the colliding run has nothing on disk — its reap converges (returns true)"
        );
        assert!(
            is_live_worktree(&wt_x),
            "a colliding TERMINAL run's reap must not remove the owner's clean live tree"
        );

        remove_worktree(p, "a:b-c:a0"); // cancel_run's force-discard spelling
        assert!(
            is_live_worktree(&wt_x),
            "a colliding CANCELLED run's force-remove must not destroy the owner's tree"
        );

        // The owner's own cleanup still works — ownership guards the tree, not the reap.
        assert!(reap_worktree_if_clean(p, "a-b:c:a0"));
        assert!(!wt_x.exists(), "the owner's keyed reap still converges");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The core#337 repro end-to-end at the id's SOURCE: the run id comes from the engine's own
    /// dispatch rule ([`crate::campaign::Campaign::derive_run_id`], §2.1) — not a hand-spelled
    /// lookalike — and provisions through [`create_worktree`]. The test also pins WHY main
    /// failed: git itself refuses the verbatim `wicked/{run_id}` branch main used to mint.
    #[test]
    fn a_dispatch_derived_campaign_run_id_provisions_through_the_engine_path() {
        let def = crate::campaign::CampaignDef {
            id: "web-refresh".into(),
            name: String::new(),
            nodes: vec![crate::campaign::CampaignNode {
                node_id: "lint".into(),
                run_spec: crate::campaign::RunSpec {
                    problem: "p".into(),
                    clis: vec![],
                    entity_mode: crate::scope::EntityMode::Shared,
                    human_confirm: crate::domain::HumanConfirm::None,
                    repo_ref: Some("r".into()),
                    workflow_id: None,
                },
            }],
            edges: vec![],
            policy: crate::campaign::FailurePolicy::default(),
            max_concurrency: 1,
        };
        crate::campaign::validate(&def).unwrap();
        let campaign = crate::campaign::Campaign::new(def);
        let rid = campaign.derive_run_id("lint");
        assert_eq!(rid, "web-refresh:lint:a0", "the §2.1 id rule");

        let root = git_repo("dispatch");
        let p = root.to_str().unwrap();
        // Main's spelling: branch `wicked/{run_id}` VERBATIM — git refuses it outright.
        let (ok, _, _) = git(
            p,
            &["check-ref-format", "--branch", &format!("wicked/{rid}")],
        )
        .unwrap();
        assert!(
            !ok,
            "precondition — the verbatim campaign branch is what git refuses"
        );

        let wt = create_worktree(p, &rid).expect("the engine provisions the campaign node");
        assert!(is_live_worktree(&wt));
        assert_eq!(
            wt.file_name().unwrap().to_str().unwrap(),
            "web-refresh-lint-a0"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
