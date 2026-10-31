//! Corroboration: how many INDEPENDENT sources in the repository support a requirement the
//! extractor stated (operator ruling 2026-10-31, "validation by duplication").
//!
//! The extractor's statement is the CLAIM. What corroborates it is evidence the extractor did not
//! write: the code that calls the symbol, the tests that exercise it, the docs that name it, and any
//! further extractor seat that independently stated a rule for it. A requirement with at least
//! [`VALIDATED_MIN_SOURCES`] such sources is `validated`; with fewer it is `unconfirmed`, and the
//! item says WHY nothing corroborates it (no caller, a single caller, only a test, a configuration
//! entry, or a file the code index skipped — wicked-estate#374's `actor.rs` gap), so a reader can
//! tell "nobody uses this" from "we could not see who uses it".
//!
//! Everything here is a deterministic read of the estate store (plus, when the caller passes the
//! repository root, `git ls-files` to find tracked source files the index never parsed). Nothing is
//! fabricated: a count is a count of distinct nodes or files, and the reason names what was found.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use wicked_apps_core::GraphRead;

/// At or above this many independent sources a requirement is `validated`.
pub const VALIDATED_MIN_SOURCES: u32 = 2;

/// The corroboration of ONE requirement, as emitted on `requirements_graph.json` (domain-model
/// 1.1.0, bundle 1.2.3, optional `requirement.corroboration`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Corroboration {
    /// `"validated"` (≥ [`VALIDATED_MIN_SOURCES`] independent sources) or `"unconfirmed"`.
    pub status: String,
    /// The independent sources counted: `call_sites + tests + docs + extra_extractors`.
    pub sources: u32,
    /// Distinct non-test code nodes that call or reference the symbol (incl. a route that serves it).
    pub call_sites: u32,
    /// Distinct test nodes (in a test file or a test module) that call or reference it.
    pub tests: u32,
    /// Distinct documentation files in the index that name it.
    pub docs: u32,
    /// Extractor seats BEYOND the first that independently stated a business rule for it.
    pub extra_extractors: u32,
    /// Why nothing (or only one thing) corroborates it. Present iff `status == "unconfirmed"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<Unconfirmed>,
}

/// The reason an item is unconfirmed: a stable `code` for filtering and a sentence for people.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unconfirmed {
    /// One of `unindexed_file`, `dead_code`, `config_only`, `single_caller`, `test_only`,
    /// `doc_only`, `extractors_only`.
    pub code: String,
    pub text: String,
}

/// Inbound edge kinds that count as "something uses this symbol". Structural edges (contains,
/// defines, imports, type links) do not: a file containing a function is not evidence the
/// function's behavior is wanted.
fn is_use_edge(kind: &wicked_apps_core::EdgeKind) -> bool {
    use wicked_apps_core::EdgeKind::*;
    match kind {
        Calls | References | Instantiates => true,
        Other(t) => matches!(t.as_str(), "invokes" | "uses" | "accesses" | "serves"),
        _ => false,
    }
}

/// Whether a path is a test file by the conventions of the languages estate indexes.
pub fn is_test_path(file: &str) -> bool {
    let f = file.replace('\\', "/").to_ascii_lowercase();
    let base = f.rsplit('/').next().unwrap_or(&f);
    f.starts_with("test/")
        || f.starts_with("tests/")
        || f.starts_with("spec/")
        || f.starts_with("__tests__/")
        || f.contains("/test/")
        || f.contains("/tests/")
        || f.contains("/__tests__/")
        || f.contains("/spec/")
        || base.contains(".test.")
        || base.contains(".spec.")
        || base.contains("_test.")
        || base.starts_with("test_")
}

/// Whether a node is test code: in a test file, or (Rust/Python inline tests) in a `tests` module
/// or named `test_*`.
fn is_test_node(node: &wicked_apps_core::Node) -> bool {
    if is_test_path(&node.location.file) {
        return true;
    }
    let sym = node.symbol.as_str();
    sym.contains("/tests/") || sym.contains("::tests::") || node.name.starts_with("test_")
}

fn is_doc_path(file: &str) -> bool {
    let f = file.to_ascii_lowercase();
    [".md", ".mdx", ".rst", ".adoc", ".txt"]
        .iter()
        .any(|e| f.ends_with(e))
}

fn is_config_path(file: &str) -> bool {
    let f = file.to_ascii_lowercase();
    [
        ".json",
        ".yaml",
        ".yml",
        ".toml",
        ".lock",
        ".ini",
        ".env",
        ".xml",
        ".properties",
    ]
    .iter()
    .any(|e| f.ends_with(e))
}

/// Names too generic for a textual mention to say anything about one symbol.
const GENERIC_NAMES: &[&str] = &[
    "add", "all", "and", "any", "app", "get", "set", "run", "new", "main", "name", "type", "data",
    "value", "test", "init", "list", "item", "items", "key", "keys", "len", "map", "for", "the",
    "use", "not", "from", "into", "with", "this", "self", "next", "call", "load", "save", "read",
    "write", "open", "close", "start", "stop", "default", "version", "config", "index", "error",
    "result", "options", "state", "id", "path", "file", "text", "info", "debug", "log",
];

fn is_meaningful_name(name: &str) -> bool {
    name.chars().count() >= 3 && !GENERIC_NAMES.contains(&name.to_ascii_lowercase().as_str())
}

/// Every identifier-shaped token of `text` (`[A-Za-z_][A-Za-z0-9_]*`).
fn identifiers(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|t| {
            t.chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        })
}

/// Repo-relative, forward slashes, no leading `./` — so the index's and git's spellings compare.
fn normalize_path(p: &str) -> String {
    let p = p.replace('\\', "/");
    p.strip_prefix("./").unwrap_or(&p).to_string()
}

/// How many skipped files are remembered per identifier (enough to skip the node's own file).
const MAX_FILES_PER_MENTION: usize = 3;

/// How many tracked-but-unindexed files are scanned, and how big one may be.
const MAX_UNINDEXED_FILES: usize = 200;
const MAX_UNINDEXED_BYTES: u64 = 8 * 1024 * 1024;

/// The store-wide facts every assessment reads, built ONCE per domain-graph build.
pub struct CorroborationIndex {
    /// target symbol → distinct (source symbol, is_test) pairs that use it.
    users: HashMap<String, BTreeSet<(String, bool)>>,
    /// identifier → how many indexed doc files name it.
    doc_mentions: HashMap<String, u32>,
    /// identifier → up to [`MAX_FILES_PER_MENTION`] tracked source files the index skipped that
    /// name it.
    unindexed_mentions: HashMap<String, Vec<String>>,
}

impl CorroborationIndex {
    /// Read the store's use edges and doc files; when `repo_root` is given, also list the tracked
    /// source files the index did not parse and record the identifiers they contain.
    pub fn build(store: &dyn GraphRead, repo_root: Option<&Path>) -> anyhow::Result<Self> {
        let nodes = store.all_nodes()?;
        let by_symbol: HashMap<&str, &wicked_apps_core::Node> =
            nodes.iter().map(|n| (n.symbol.as_str(), n)).collect();
        let mut users: HashMap<String, BTreeSet<(String, bool)>> = HashMap::new();
        // Rust's inline tests: the index does not model `mod tests`, so a test function there has a
        // plain file-level symbol and any name. Where the file's stored text has a `#[cfg(test)]`,
        // a node that starts after it is test code (per file, read once).
        let mut cfg_test_at: HashMap<String, Option<u32>> = HashMap::new();
        let mut inline_test = |n: &wicked_apps_core::Node| -> bool {
            if !n.location.file.ends_with(".rs") {
                return false;
            }
            let at = cfg_test_at
                .entry(n.location.file.clone())
                .or_insert_with(|| {
                    store
                        .file_content(&n.location.file)
                        .ok()
                        .flatten()
                        .and_then(|t| t.find("#[cfg(test)]"))
                        .and_then(|i| u32::try_from(i).ok())
                });
            at.is_some_and(|at| n.location.span.start_byte > at)
        };
        for e in store.all_edges()? {
            if !is_use_edge(&e.kind) || e.source == e.target {
                continue;
            }
            let is_test = by_symbol
                .get(e.source.as_str())
                .is_some_and(|n| is_test_node(n) || inline_test(n));
            users
                .entry(e.target.as_str().to_string())
                .or_default()
                .insert((e.source.as_str().to_string(), is_test));
        }

        let indexed = store.indexed_files()?;
        let mut doc_mentions: HashMap<String, u32> = HashMap::new();
        for file in indexed.iter().filter(|f| is_doc_path(f)) {
            let Some(text) = store.file_content(file)? else {
                continue;
            };
            let seen: HashSet<&str> = identifiers(&text).collect();
            for id in seen {
                *doc_mentions.entry(id.to_string()).or_insert(0) += 1;
            }
        }

        let unindexed_mentions = match repo_root {
            Some(root) => unindexed_mentions(root, &indexed),
            None => HashMap::new(),
        };
        Ok(CorroborationIndex {
            users,
            doc_mentions,
            unindexed_mentions,
        })
    }

    /// Assess one requirement's code node. `extractor_sources` is the number of DISTINCT extractor
    /// provenances that stated a business rule for it (0 when only a RISK flag exists).
    pub fn assess(&self, node: &wicked_apps_core::Node, extractor_sources: u32) -> Corroboration {
        let users = self.users.get(node.symbol.as_str());
        let tests = users.map_or(0, |u| u.iter().filter(|(_, t)| *t).count()) as u32;
        let call_sites = users.map_or(0, |u| u.iter().filter(|(_, t)| !*t).count()) as u32;
        let docs = if is_meaningful_name(&node.name) {
            self.doc_mentions.get(&node.name).copied().unwrap_or(0)
        } else {
            0
        };
        let extra_extractors = extractor_sources.saturating_sub(1);
        let sources = call_sites + tests + docs + extra_extractors;
        if sources >= VALIDATED_MIN_SOURCES {
            return Corroboration {
                status: "validated".to_string(),
                sources,
                call_sites,
                tests,
                docs,
                extra_extractors,
                why: None,
            };
        }
        let why = self.why_unconfirmed(node, call_sites, tests, docs, extra_extractors);
        Corroboration {
            status: "unconfirmed".to_string(),
            sources,
            call_sites,
            tests,
            docs,
            extra_extractors,
            why: Some(why),
        }
    }

    fn why_unconfirmed(
        &self,
        node: &wicked_apps_core::Node,
        call_sites: u32,
        tests: u32,
        docs: u32,
        extra_extractors: u32,
    ) -> Unconfirmed {
        let mk = |code: &str, text: String| Unconfirmed {
            code: code.to_string(),
            text,
        };
        // A skipped file that names the symbol explains a missing caller better than anything else:
        // the caller may well exist, the index just never saw it.
        if is_meaningful_name(&node.name) {
            // Never the node's OWN file: a definition names itself, and a path spelled differently
            // in the index than in git must not turn that into a phantom caller.
            let own = normalize_path(&node.location.file);
            let other = self
                .unindexed_mentions
                .get(&node.name)
                .and_then(|files| files.iter().find(|f| normalize_path(f) != own));
            if let Some(file) = other {
                return mk(
                    "unindexed_file",
                    format!(
                        "{file} names it, but the code index skipped that file, so callers there \
                         are not counted."
                    ),
                );
            }
        }
        if call_sites == 1 {
            return mk(
                "single_caller",
                "One caller, and no test or doc touches it.".to_string(),
            );
        }
        if tests == 1 {
            return mk(
                "test_only",
                "Only one test touches it; nothing else in the indexed code calls it.".to_string(),
            );
        }
        if docs == 1 {
            return mk(
                "doc_only",
                "Only a doc names it; no caller or test in the indexed code.".to_string(),
            );
        }
        if extra_extractors >= 1 {
            return mk(
                "extractors_only",
                "Only the extractors agree on it; no caller, test or doc corroborates it."
                    .to_string(),
            );
        }
        if is_config_path(&node.location.file) {
            return mk(
                "config_only",
                "A configuration entry: no code, test or doc in the index refers to it."
                    .to_string(),
            );
        }
        mk(
            "dead_code",
            "No caller, test or doc refers to it in the indexed code: dead code, or an entry point \
             reached only from outside the repository."
                .to_string(),
        )
    }
}

/// Tracked source files (by the extensions the index DID parse elsewhere in this repo) that the
/// index holds no record of, and the identifiers each names. `git ls-files` failing (not a git
/// checkout, git missing) yields an empty map: the reason is then simply never offered.
fn unindexed_mentions(root: &Path, indexed: &[String]) -> HashMap<String, Vec<String>> {
    use wicked_apps_core::HardenedCommand;
    let indexed_set: HashSet<String> = indexed.iter().map(|f| normalize_path(f)).collect();
    let code_exts: HashSet<String> = indexed
        .iter()
        .filter(|f| !is_doc_path(f) && !is_config_path(f))
        .filter_map(|f| {
            Path::new(f)
                .extension()
                .map(|e| e.to_string_lossy().to_string())
        })
        .collect();
    let out = match std::process::Command::new("git")
        .hardened()
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z"])
        .output()
    {
        Ok(o) if o.status.success() => o.stdout,
        _ => return HashMap::new(),
    };
    let mut mentions: HashMap<String, Vec<String>> = HashMap::new();
    let tracked = String::from_utf8_lossy(&out);
    let mut scanned = 0usize;
    for file in tracked.split('\0').filter(|f| !f.is_empty()) {
        if indexed_set.contains(&normalize_path(file)) {
            continue;
        }
        let Some(ext) = Path::new(file).extension().map(|e| e.to_string_lossy()) else {
            continue;
        };
        if !code_exts.contains(ext.as_ref()) {
            continue;
        }
        if scanned >= MAX_UNINDEXED_FILES {
            break;
        }
        let path = root.join(file);
        if std::fs::metadata(&path).map_or(true, |m| m.len() > MAX_UNINDEXED_BYTES) {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        scanned += 1;
        let text = String::from_utf8_lossy(&bytes);
        for id in identifiers(&text).collect::<HashSet<_>>() {
            let files = mentions.entry(id.to_string()).or_default();
            if files.len() < MAX_FILES_PER_MENTION {
                files.push(file.to_string());
            }
        }
    }
    mentions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_paths_by_convention() {
        for p in [
            "test/math.test.js",
            "tests/cli.rs",
            "pkg/__tests__/a.ts",
            "src/a.spec.ts",
            "go/x_test.go",
            "py/test_x.py",
        ] {
            assert!(is_test_path(p), "{p}");
        }
        for p in ["src/math.js", "src/attest.rs", "README.md"] {
            assert!(!is_test_path(p), "{p}");
        }
    }

    #[test]
    fn generic_names_never_count_as_mentions() {
        assert!(!is_meaningful_name("main"));
        assert!(!is_meaningful_name("id"));
        assert!(is_meaningful_name("clamp"));
        assert!(is_meaningful_name("sum"));
    }

    /// tiny-math's shape: `sum` is called by `mean` and by the test file, `mean` only by the test
    /// file, `clamp` by the test file and named in the README, a `package.json` key by nothing, `orphan` by nothing, `helper` by one caller.
    #[test]
    fn counts_independent_sources_and_names_why_an_item_is_unconfirmed() {
        use wicked_apps_core::{
            synthetic_symbol, Edge, EdgeKind, GraphWrite, Language, Location, Node, NodeKind,
            ResolutionTier, Span, SqliteStore, SYMBOL_SCHEME,
        };
        let mk = |name: &str, kind: NodeKind, file: &str| {
            Node::new(
                synthetic_symbol("code", name),
                kind,
                name.to_string(),
                Language::new(SYMBOL_SCHEME),
                Location::new(file.to_string(), Span::ZERO),
            )
        };
        let sum = mk("sum", NodeKind::Function, "src/math.js");
        let mean = mk("mean", NodeKind::Function, "src/math.js");
        let clamp = mk("clamp", NodeKind::Function, "src/math.js");
        let helper = mk("helper", NodeKind::Function, "src/util.js");
        let orphan = mk("orphan", NodeKind::Function, "src/util.js");
        let pkg = mk("scripts", NodeKind::Struct, "package.json");
        let tests = mk("math.test.js", NodeKind::File, "test/math.test.js");
        let mut st = SqliteStore::in_memory().unwrap();
        st.begin_batch().unwrap();
        st.upsert_nodes(&[
            sum.clone(),
            mean.clone(),
            clamp.clone(),
            helper.clone(),
            orphan.clone(),
            pkg.clone(),
            tests.clone(),
        ])
        .unwrap();
        let call = |a: &Node, b: &Node| {
            Edge::new(
                a.symbol.clone(),
                b.symbol.clone(),
                EdgeKind::Calls,
                ResolutionTier::Parsed,
                "t",
            )
        };
        st.upsert_edges(&[
            call(&mean, &sum),
            call(&tests, &sum),
            call(&tests, &mean),
            call(&tests, &clamp),
            call(&sum, &helper),
            // Structural edges are not use: a file containing a key does not corroborate it.
            Edge::new(
                tests.symbol.clone(),
                orphan.symbol.clone(),
                EdgeKind::Contains,
                ResolutionTier::Parsed,
                "t",
            ),
        ])
        .unwrap();
        // A doc that names `clamp` (and a generic word that must not count for anything).
        st.set_file_digest("README.md", "r1").unwrap();
        st.set_file_content("README.md", "Use `clamp` to bound a value; see main.")
            .unwrap();
        st.commit_batch().unwrap();

        let idx = CorroborationIndex::build(&st, None).unwrap();
        let s = idx.assess(&sum, 1);
        assert_eq!(
            (s.status.as_str(), s.sources, s.call_sites, s.tests),
            ("validated", 2, 1, 1)
        );
        assert!(s.why.is_none());
        let why = |n: &Node| idx.assess(n, 1).why.map(|w| w.code).unwrap_or_default();
        assert_eq!(why(&mean), "test_only");
        let c = idx.assess(&clamp, 1);
        assert_eq!((c.status.as_str(), c.tests, c.docs), ("validated", 1, 1));
        assert_eq!(why(&helper), "single_caller");
        assert_eq!(why(&orphan), "dead_code");
        assert_eq!(why(&pkg), "config_only");
        // A second extractor seat agreeing is a source of its own.
        let two = idx.assess(&mean, 2);
        assert_eq!(
            (two.status.as_str(), two.extra_extractors),
            ("validated", 1)
        );
    }

    /// wicked-estate#374's shape: a tracked source file the index skipped names the symbol, so its
    /// missing callers are the index's blind spot, not dead code — and the item says which file.
    #[test]
    fn a_skipped_file_that_names_the_symbol_explains_the_missing_callers() {
        use wicked_apps_core::{
            synthetic_symbol, GraphWrite, Language, Location, Node, NodeKind, Span, SqliteStore,
            SYMBOL_SCHEME,
        };
        let root = std::env::temp_dir().join(format!("corr-unindexed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn settle_invoice() {}").unwrap();
        std::fs::write(root.join("src/actor.rs"), "fn tick() { settle_invoice(); }").unwrap();
        let git = |args: &[&str]| {
            use wicked_apps_core::HardenedCommand;
            std::process::Command::new("git")
                .hardened()
                .arg("-C")
                .arg(&root)
                .args(args)
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        git(&["add", "."]);
        let node = Node::new(
            synthetic_symbol("code", "settle_invoice"),
            NodeKind::Function,
            "settle_invoice".to_string(),
            Language::new(SYMBOL_SCHEME),
            Location::new("src/lib.rs".to_string(), Span::ZERO),
        );
        let mut st = SqliteStore::in_memory().unwrap();
        st.begin_batch().unwrap();
        st.upsert_nodes(std::slice::from_ref(&node)).unwrap();
        st.set_file_digest("src/lib.rs", "d1").unwrap();
        st.commit_batch().unwrap();

        let idx = CorroborationIndex::build(&st, Some(&root)).unwrap();
        let c = idx.assess(&node, 1);
        let why = c.why.expect("unconfirmed");
        assert_eq!(why.code, "unindexed_file");
        assert!(
            why.text.starts_with("src/actor.rs names it"),
            "{}",
            why.text
        );
        // Without the root the reason degrades honestly to "nothing refers to it".
        let bare = CorroborationIndex::build(&st, None)
            .unwrap()
            .assess(&node, 1);
        assert_eq!(bare.why.unwrap().code, "dead_code");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn identifiers_split_on_punctuation() {
        let ids: Vec<&str> = identifiers("helpers (sum, mean, clamp). Run `npm test` 3x").collect();
        assert_eq!(
            ids,
            vec!["helpers", "sum", "mean", "clamp", "Run", "npm", "test"]
        );
    }
}
