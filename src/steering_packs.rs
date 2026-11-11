//! (core#804) The guidance steering packs the engine's shipped workflows cite, seeded into the
//! store at boot so `rules.recall` finds them without an operator `rules ingest`.
//!
//! The `mcp-server` workflow's design and review phases are told to check their work against
//! MCPS-1001..1007, and every governed review loop against RVWL-1001..1004. Both packs were opt-in
//! (`wicked-core rules ingest governance/packs/<pack>` or Steering → Import), and nothing at launch
//! said so: on the rig the store held 14 rules and none of them, the design unit's recall came back
//! empty, and the seat invented meanings for rule ids it could not read (run `c3aa0bfb`).
//! `stack-builtins` (POL-2101, operator ruling 2026-10-12: check what the chosen stack already
//! provides before writing code) seeds the same way, so every build and review phase can recall it
//! on a fresh daemon. Its doc lives only here; the AW-13 seed run stages it beside the corpus.
//!
//! The docs are EMBEDDED (`include_bytes!`, the shipped binary has no pack directory) and parsed
//! by the same markdown path `rules ingest` uses ([`wicked_governance::EmbeddedMarkdownAdapter`]),
//! under the same root-relative file names, so the provenance (`<file>@<git blob sha>#<RULE-ID>`)
//! of a seeded rule equals the one an ingest of `governance/packs/<pack>` writes. INSERT-ONLY like
//! the `mcp-defaults` / `editor-defaults` seeds: a rule already in the store — re-ingested from a
//! newer doc, edited, or RETIRED by the operator — is left exactly as it is, so a restart never
//! resurrects a retired rule. A NEWER doc (a release that rewords a rule) reaches an existing
//! store through `rules ingest governance/packs/<pack>`, which re-registers these ids as it always
//! did — they are deliberately not in `boot_seeded_rule_ids` (core#709 keeps those ledger rows as
//! stored), because a guidance pack's doc, not the store, is its source of truth.
//! Written straight to the store with one autocommit upsert (no batch,
//! no bus event) because this runs on the actor's boot path (core#705, `tests/bus_handoff.rs`).

use wicked_apps_core::ToNode;

/// One shipped pack: its directory name under `governance/packs/`, its docs, root-relative, and
/// the runs its rules are in force on (core#1414).
struct ShippedPack {
    name: &'static str,
    docs: &'static [(&'static str, &'static [u8])],
    fits: PackFit,
}

/// (core#1414) Which runs a shipped pack's rules are in force on. A rule of a pack that does not fit
/// the run, by fact, never enters the run's rule set: it is not judged and not skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PackFit {
    /// Every run (`review-loop`, `stack-builtins`).
    Any,
    /// Runs that build an MCP server (`mcp-server`: "defaults for MCP servers the `mcp-server`
    /// workflow builds"): a run launched from the `mcp-server` preset, or a change in a codebase
    /// that builds one ([`builds_mcp_server`]).
    McpServer,
}

/// The preset the `mcp-server` workflow launches as.
pub(crate) const MCP_SERVER_PRESET: &str = "mcp-server";

/// (core#1414) What is known about the run a pack's fit is decided on. `None` = not known (it
/// removes nothing).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RunFit {
    /// The preset the launch named: `Some(None)` = the run's record shows it named none.
    pub preset: Option<Option<String>>,
    /// Whether the change is in a codebase that builds an MCP server ([`builds_mcp_server`]).
    pub builds_mcp_server: Option<bool>,
}

/// (core#1414) Why `rule` is out of the run's rule set: it is a shipped pack rule AS SHIPPED whose
/// pack does not fit the run, by fact. `None` = it stays (it fits, or a fact is unknown, or it is
/// not a shipped rule as shipped: an operator-edited or broadened pack rule is judged from its
/// stored meaning, wherever it is).
pub(crate) fn out_of_scope(
    rule: &wicked_governance::ConformanceRule,
    run: &RunFit,
) -> Option<&'static str> {
    match pack_overlay(rule)?.fits {
        PackFit::Any => None,
        PackFit::McpServer => {
            let other_preset = matches!(
                &run.preset,
                Some(p) if p.as_deref() != Some(MCP_SERVER_PRESET)
            );
            (other_preset && run.builds_mcp_server == Some(false)).then_some(
                "the mcp-server pack: this run is not the mcp-server workflow and its change is not \
                 in a codebase that builds an MCP server (no manifest above a changed file declares \
                 an MCP server SDK, and the change imports none)",
            )
        }
    }
}

/// The manifests whose dependencies say a codebase builds an MCP server.
const MANIFESTS: &[&str] = &[
    "package.json",
    "pyproject.toml",
    "requirements.txt",
    "Cargo.toml",
    "go.mod",
];

/// The most bytes of one manifest read.
const MANIFEST_CAP: u64 = 1024 * 1024;
/// The most directories looked at for manifests.
const MAX_MANIFEST_DIRS: usize = 256;

/// Whether a manifest's text declares an MCP server SDK as a dependency.
fn manifest_declares_mcp(name: &str, text: &str) -> bool {
    match name {
        "package.json" => {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
                // Unparseable: read it as text (a dependency key is a quoted name).
                return text.contains("\"@modelcontextprotocol/")
                    || text.contains("\"fastmcp\"")
                    || text.contains("\"mcp-framework\"");
            };
            [
                "dependencies",
                "devDependencies",
                "peerDependencies",
                "optionalDependencies",
            ]
            .iter()
            .filter_map(|k| v.get(*k).and_then(|d| d.as_object()))
            .flat_map(|d| d.keys())
            .any(|k| {
                k.starts_with("@modelcontextprotocol/") || k == "fastmcp" || k == "mcp-framework"
            })
        }
        "pyproject.toml" | "requirements.txt" => {
            // A requirement named `mcp` or `fastmcp` (with extras or a version spec), quoted in
            // pyproject or at the start of a requirements line.
            static PY: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
            PY.get_or_init(|| {
                regex::Regex::new(
                    r#"(?mi)(^|["'])\s*(fastmcp|mcp)\s*(\[[^\]]*\])?\s*([<>=!~;,"']|$)"#,
                )
                .expect("a valid pattern")
            })
            .is_match(text)
        }
        "Cargo.toml" => {
            static RS: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
            RS.get_or_init(|| {
                regex::Regex::new(r"(?m)^\s*(rmcp|rust-mcp-sdk)\s*(=|\.)").expect("a valid pattern")
            })
            .is_match(text)
        }
        "go.mod" => {
            text.contains("github.com/mark3labs/mcp-go")
                || text.contains("github.com/modelcontextprotocol/go-sdk")
        }
        _ => false,
    }
}

/// Whether an added line of the change imports an MCP server SDK.
fn diff_imports_mcp(diff: &str) -> bool {
    static IMPORT: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = IMPORT.get_or_init(|| {
        regex::Regex::new(
            r#"(@modelcontextprotocol/sdk|["']fastmcp["']|\b(from|import)\s+fastmcp\b|\bfrom\s+mcp(\.server)?(\.fastmcp)?\s+import\b|\bimport\s+mcp(\.server)?\b|\brmcp::|github\.com/mark3labs/mcp-go|github\.com/modelcontextprotocol/go-sdk)"#,
        )
        .expect("a valid pattern")
    });
    diff.lines()
        .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
        .any(|l| re.is_match(l))
}

/// (core#1414) Whether the change at `wd` is in a codebase that builds an MCP server: a manifest at
/// the worktree root or in any directory above a changed file declares an MCP server SDK, or an
/// added line imports one. `None` when a manifest exists and cannot be read (unknown removes
/// nothing). Only paths inside the worktree are looked at.
pub(crate) fn builds_mcp_server(
    wd: &std::path::Path,
    paths: &[String],
    diff: &str,
) -> Option<bool> {
    if diff_imports_mcp(diff) {
        return Some(true);
    }
    let mut dirs: std::collections::BTreeSet<std::path::PathBuf> =
        std::collections::BTreeSet::new();
    dirs.insert(std::path::PathBuf::new());
    for p in paths {
        let rel = std::path::Path::new(p);
        // A path that climbs out of the worktree (or is absolute) names no directory of it.
        if rel.is_absolute()
            || rel
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            continue;
        }
        let mut d = rel.parent();
        while let Some(dir) = d {
            dirs.insert(dir.to_path_buf());
            d = dir.parent();
        }
    }
    if dirs.len() > MAX_MANIFEST_DIRS {
        return None;
    }
    let mut unknown = false;
    for dir in &dirs {
        for name in MANIFESTS {
            let file = wd.join(dir).join(name);
            let Ok(meta) = std::fs::symlink_metadata(&file) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            use std::io::Read;
            let mut text = String::new();
            let read = std::fs::File::open(&file)
                .and_then(|f| f.take(MANIFEST_CAP).read_to_string(&mut text));
            match read {
                Ok(_) if manifest_declares_mcp(name, &text) => return Some(true),
                Ok(_) => {}
                Err(_) => unknown = true,
            }
        }
    }
    (!unknown).then_some(false)
}

macro_rules! pack_doc {
    ($pack:literal, $file:literal) => {
        (
            $file,
            include_bytes!(concat!("../governance/packs/", $pack, "/", $file)) as &[u8],
        )
    };
}

/// The packs seeded at boot. Guidance packs only: an enforcing (`policy`) pack changes what the
/// tool and MCP gates deny and stays an operator's explicit ingest. (DES-RULE-CHECK-001 rev 2 Part
/// B) The review gate's rule check judges these too: a broken MCPS-* or POL-2101 rule denies like
/// any rule; an RVWL-* process rule is reported and never blocks ([`pack_overlay`]).
const SHIPPED_PACKS: &[ShippedPack] = &[
    ShippedPack {
        name: "mcp-server",
        fits: PackFit::McpServer,
        docs: &[
            pack_doc!("mcp-server", "mcp-server-authentication.md"),
            pack_doc!("mcp-server", "mcp-server-contract-tests.md"),
            pack_doc!("mcp-server", "mcp-server-input-and-egress.md"),
            pack_doc!("mcp-server", "mcp-server-language.md"),
            pack_doc!("mcp-server", "mcp-server-license-and-egress.md"),
            pack_doc!("mcp-server", "mcp-server-logging.md"),
            pack_doc!("mcp-server", "mcp-server-telemetry.md"),
        ],
    },
    ShippedPack {
        name: "review-loop",
        fits: PackFit::Any,
        docs: &[
            pack_doc!("review-loop", "review-loop-bounded-rework.md"),
            pack_doc!("review-loop", "review-loop-foreground-evidence.md"),
            pack_doc!("review-loop", "review-loop-one-slice-intent.md"),
            pack_doc!("review-loop", "review-loop-stateful-verdicts.md"),
        ],
    },
    ShippedPack {
        name: "stack-builtins",
        fits: PackFit::Any,
        docs: &[pack_doc!("stack-builtins", "stack-builtins.md")],
    },
];

/// Every rule the shipped packs declare, parsed through the one markdown path.
fn shipped_rules() -> anyhow::Result<Vec<wicked_governance::ConformanceRule>> {
    let mut rules = Vec::new();
    for pack in SHIPPED_PACKS {
        let adapter = wicked_governance::EmbeddedMarkdownAdapter::new(
            pack.docs
                .iter()
                .map(|(file, bytes)| ((*file).to_string(), bytes.to_vec())),
        );
        let parsed = wicked_governance::ingest_from(&adapter)
            .map_err(|e| anyhow::anyhow!("steering pack {}: {e}", pack.name))?;
        rules.extend(parsed);
    }
    Ok(rules)
}

/// (DES-RULE-CHECK-001 rev 2, Part B) Who a PROCESS rule's conduct belongs to: reported with the
/// row, never a deny or a hold (Q6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PackParty {
    /// The evaluator's conduct (RVWL-1001).
    Reviewer,
    /// The harness: worker-turn lifecycle, the rework cap (RVWL-1002, RVWL-1003).
    Engine,
    /// The person's intent (RVWL-1004).
    Person,
}

impl PackParty {
    /// The wire spelling of `RuleVerdict.party`.
    pub(crate) fn as_wire(self) -> &'static str {
        match self {
            PackParty::Reviewer => "reviewer",
            PackParty::Engine => "engine",
            PackParty::Person => "person",
        }
    }
}

/// (Part B, B.2) How the rule check judges a shipped pack rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PackKind {
    /// A work rule judged on the change, like any rule (MCPS-*).
    Work,
    /// POL-2101: its substance judged on the change (no creator prose), its record clause judged
    /// apart on the creator steps' records; the engine combines the two.
    WorkWithRecord,
    /// A process rule judged on the engine's record, reported with its party, never blocking.
    Process(PackParty),
}

/// (Part B, B.1) What the engine attaches to a shipped pack rule at check time: its kind and its
/// pack doc's title (the scope its statement does not carry). Applied only to CANONICAL content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PackOverlay {
    pub kind: PackKind,
    /// The pack doc's own title, e.g. `MCP servers: structured logging`.
    pub scope: String,
    /// The shipped rule's content key with no overlay ([`crate::rule_check::canonical_key`]).
    canonical: String,
    /// (core#1414) The runs its pack's rules are in force on.
    pub fits: PackFit,
}

/// The kind of each shipped rule id the table knows. A shipped rule missing here gets no overlay
/// (judged from its stored meaning); the test below pins that every shipped id is listed.
fn kind_of(id: &str) -> Option<PackKind> {
    Some(match id {
        "MCPS-1001" | "MCPS-1002" | "MCPS-1003" | "MCPS-1004" | "MCPS-1005" | "MCPS-1006"
        | "MCPS-1007" => PackKind::Work,
        "POL-2101" => PackKind::WorkWithRecord,
        "RVWL-1001" => PackKind::Process(PackParty::Reviewer),
        "RVWL-1002" | "RVWL-1003" => PackKind::Process(PackParty::Engine),
        "RVWL-1004" => PackKind::Process(PackParty::Person),
        _ => return None,
    })
}

/// A doc's frontmatter `title`, unquoted.
fn doc_title(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    let front = text.strip_prefix("---")?;
    let end = front.find("\n---")?;
    front[..end].lines().find_map(|l| {
        let t = l.strip_prefix("title:")?.trim();
        let t = t
            .strip_prefix('"')
            .and_then(|t| t.strip_suffix('"'))
            .unwrap_or(t);
        (!t.is_empty()).then(|| t.to_string())
    })
}

/// The table, built once from the embedded docs: rule id → its overlay.
fn table() -> &'static std::collections::BTreeMap<String, PackOverlay> {
    static TABLE: std::sync::OnceLock<std::collections::BTreeMap<String, PackOverlay>> =
        std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut out = std::collections::BTreeMap::new();
        for rule in shipped_rules().unwrap_or_default() {
            let Some(kind) = kind_of(&rule.id) else {
                continue;
            };
            // `<file>@<blob sha>#<RULE-ID>`: the doc the rule was parsed from.
            let file = rule
                .provenance
                .reference
                .as_deref()
                .unwrap_or_default()
                .split('@')
                .next()
                .unwrap_or_default()
                .to_string();
            let Some((fits, scope)) = SHIPPED_PACKS.iter().find_map(|p| {
                p.docs
                    .iter()
                    .find(|(f, _)| *f == file)
                    .and_then(|(_, bytes)| doc_title(bytes))
                    .map(|t| (p.fits, t))
            }) else {
                continue;
            };
            out.insert(
                rule.id.clone(),
                PackOverlay {
                    kind,
                    scope,
                    canonical: crate::rule_check::canonical_key(&rule),
                    fits,
                },
            );
        }
        out
    })
}

/// (DES-RULE-CHECK-001 rev 2, Part B) The overlay for `rule`, when it is a shipped pack rule AS
/// SHIPPED: its id is in the table and its content key (statement, criteria, obligations, facets;
/// no overlay) equals the shipped rule's. An operator-edited or broadened pack rule gets none and
/// is judged from its stored meaning (codex r2 15).
pub(crate) fn pack_overlay(
    rule: &wicked_governance::ConformanceRule,
) -> Option<&'static PackOverlay> {
    table()
        .get(&rule.id)
        .filter(|o| o.canonical == crate::rule_check::canonical_key(rule))
}

/// Every rule the shipped packs declare (the tests seed and match against them).
#[cfg(test)]
pub(crate) fn shipped_pack_rules() -> Vec<wicked_governance::ConformanceRule> {
    shipped_rules().expect("the shipped packs parse")
}

/// Seed the shipped guidance packs INSERT-ONLY. Returns how many rules were inserted.
pub(crate) fn seed_shipped_packs(
    store: &mut dyn wicked_apps_core::GraphStore,
) -> anyhow::Result<usize> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut nodes = Vec::new();
    for mut rule in shipped_rules()? {
        let symbol =
            wicked_apps_core::synthetic_symbol(wicked_governance::CONFORMANCE_RULE, &rule.id);
        if store.get_node(&symbol)?.is_some() {
            continue;
        }
        rule.validate()?;
        rule.created_at = Some(now);
        nodes.push(rule.to_node());
    }
    if nodes.is_empty() {
        return Ok(0);
    }
    store.upsert_nodes(&nodes)?;
    Ok(nodes.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wicked_apps_core::{open_store, FromNode, GraphStore};

    fn pack_dir(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("governance/packs")
            .join(name)
    }

    /// The embedded list is the pack directory: a doc added to a pack without being added here
    /// would silently never seed. And the seeded rules are byte-for-byte the ones `rules ingest`
    /// writes from the directory (same ids, statements and provenance refs).
    #[test]
    fn the_embedded_packs_match_their_directories_and_the_ingest_path() {
        for pack in SHIPPED_PACKS {
            let mut on_disk: Vec<String> = std::fs::read_dir(pack_dir(pack.name))
                .unwrap()
                .filter_map(|e| {
                    let n = e.unwrap().file_name().to_string_lossy().into_owned();
                    n.ends_with(".md").then_some(n)
                })
                .collect();
            on_disk.sort();
            let mut embedded: Vec<String> =
                pack.docs.iter().map(|(f, _)| (*f).to_string()).collect();
            embedded.sort();
            assert_eq!(embedded, on_disk, "pack {} embeds every doc", pack.name);
        }
        let mut from_dirs = Vec::new();
        for pack in SHIPPED_PACKS {
            from_dirs.extend(
                wicked_governance::ingest_from(&wicked_governance::MarkdownAdapter::new(pack_dir(
                    pack.name,
                )))
                .unwrap(),
            );
        }
        let key = |r: &wicked_governance::ConformanceRule| {
            (
                r.id.clone(),
                r.statement.clone(),
                r.provenance.reference.clone(),
            )
        };
        let mut a: Vec<_> = shipped_rules().unwrap().iter().map(key).collect();
        let mut b: Vec<_> = from_dirs.iter().map(key).collect();
        a.sort();
        b.sort();
        assert_eq!(a, b);
        let ids: Vec<&str> = a.iter().map(|(id, _, _)| id.as_str()).collect();
        for want in [
            "MCPS-1001",
            "MCPS-1007",
            "RVWL-1001",
            "RVWL-1004",
            "POL-2101",
        ] {
            assert!(ids.contains(&want), "{want} ships: {ids:?}");
        }
    }

    /// (Part B) Every shipped rule has a table row with its doc's title as its scope, and the row
    /// applies only to the rule as shipped.
    #[test]
    fn every_shipped_rule_has_an_overlay_with_its_doc_title() {
        let rules = shipped_rules().unwrap();
        assert_eq!(
            rules.len(),
            12,
            "MCPS-1001..1007, RVWL-1001..1004, POL-2101"
        );
        for r in &rules {
            let o = pack_overlay(r).unwrap_or_else(|| panic!("{} has an overlay", r.id));
            assert!(!o.scope.is_empty() && !o.scope.contains('"'), "{}", o.scope);
        }
        let logging = rules.iter().find(|r| r.id == "MCPS-1003").unwrap();
        assert_eq!(
            pack_overlay(logging).unwrap().scope,
            "MCP servers: structured logging"
        );
        assert_eq!(
            pack_overlay(rules.iter().find(|r| r.id == "RVWL-1003").unwrap())
                .unwrap()
                .kind,
            PackKind::Process(PackParty::Engine)
        );
        // (crew#1394, pi) The no-agent-code fact is POL-2101's: it is the only record-split rule.
        let split: Vec<&str> = rules
            .iter()
            .filter(|r| pack_overlay(r).unwrap().kind == PackKind::WorkWithRecord)
            .map(|r| r.id.as_str())
            .collect();
        assert_eq!(split, ["POL-2101"]);
        let mut edited = logging.clone();
        edited.statement.push_str(" Also every server.");
        assert!(
            pack_overlay(&edited).is_none(),
            "an edited rule: no overlay"
        );
    }

    /// Insert-only: a second boot inserts nothing, and a rule the operator retired stays retired.
    #[test]
    fn the_seed_is_insert_only_and_never_resurrects_a_retired_rule() {
        let mut owned = open_store(Some(":memory:")).unwrap();
        let store: &mut dyn GraphStore = &mut owned;
        let first = seed_shipped_packs(store).unwrap();
        assert_eq!(first, shipped_rules().unwrap().len());
        assert_eq!(seed_shipped_packs(store).unwrap(), 0, "insert-only");

        let symbol =
            wicked_apps_core::synthetic_symbol(wicked_governance::CONFORMANCE_RULE, "MCPS-1001");
        let node = store.get_node(&symbol).unwrap().expect("seeded");
        let mut rule = wicked_governance::ConformanceRule::from_node(&node).unwrap();
        rule.retired = true;
        store.upsert_nodes(&[rule.to_node()]).unwrap();
        assert_eq!(seed_shipped_packs(store).unwrap(), 0);
        let after = store.get_node(&symbol).unwrap().unwrap();
        assert!(
            wicked_governance::ConformanceRule::from_node(&after)
                .unwrap()
                .retired,
            "a retired rule stays retired across a re-seed"
        );
    }

    /// (core#1414) Only a shipped mcp-server rule AS SHIPPED leaves the rule set, and only when
    /// both facts are known and neither fits: another preset (or none) and no MCP server built.
    #[test]
    fn only_a_shipped_mcp_rule_leaves_and_only_on_known_facts() {
        let rules = shipped_rules().unwrap();
        let mcps = rules.iter().find(|r| r.id == "MCPS-1007").unwrap();
        let pol = rules.iter().find(|r| r.id == "POL-2101").unwrap();
        let rvwl = rules.iter().find(|r| r.id == "RVWL-1001").unwrap();
        let fit = |preset: Option<Option<&str>>, builds: Option<bool>| RunFit {
            preset: preset.map(|p| p.map(str::to_string)),
            builds_mcp_server: builds,
        };
        let out = fit(Some(Some("feature")), Some(false));
        assert!(out_of_scope(mcps, &out).is_some());
        assert!(out_of_scope(mcps, &fit(Some(None), Some(false))).is_some());
        assert!(
            out_of_scope(pol, &out).is_none(),
            "stack-builtins fits every run"
        );
        assert!(
            out_of_scope(rvwl, &out).is_none(),
            "review-loop fits every run"
        );
        for stays in [
            fit(Some(Some(MCP_SERVER_PRESET)), Some(false)),
            fit(Some(Some("feature")), Some(true)),
            fit(None, Some(false)),
            fit(Some(Some("feature")), None),
        ] {
            assert!(out_of_scope(mcps, &stays).is_none(), "{stays:?}");
        }
        // An operator-edited (or broadened) MCPS rule is judged from its stored meaning: never
        // removed by the shipped pack's scope.
        let mut edited = mcps.clone();
        edited
            .statement
            .push_str(" Every service in this org, MCP or not.");
        assert!(out_of_scope(&edited, &out).is_none());
    }

    fn scope_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("pack-scope-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// (core#1414) What says a codebase builds an MCP server: a manifest above a changed file that
    /// depends on an MCP server SDK (per ecosystem), or an added line importing one. A keyword, a
    /// description or a similarly named package says nothing.
    #[test]
    fn a_codebase_builds_an_mcp_server_by_its_manifests_or_its_imports() {
        let d = scope_dir("manifests");
        std::fs::create_dir_all(d.join("packages/crew/src")).unwrap();
        std::fs::write(
            d.join("package.json"),
            r#"{"name":"root","keywords":["mcp"],"description":"MCP-server jobs","devDependencies":{"typescript":"5"}}"#,
        )
        .unwrap();
        std::fs::write(
            d.join("packages/crew/package.json"),
            r#"{"name":"crew","dependencies":{"fastify":"4","mcp-proxy-lite":"1"}}"#,
        )
        .unwrap();
        let paths = vec!["packages/crew/src/tracker.ts".to_string()];
        assert_eq!(
            builds_mcp_server(&d, &paths, "+const x = 1;\n"),
            Some(false)
        );
        // The nearest package depends on the SDK.
        std::fs::write(
            d.join("packages/crew/package.json"),
            r#"{"name":"crew","dependencies":{"@modelcontextprotocol/sdk":"1.20.0"}}"#,
        )
        .unwrap();
        assert_eq!(builds_mcp_server(&d, &paths, ""), Some(true));
        // A manifest beside an unrelated path is not read for this change.
        let other = vec!["docs/readme.md".to_string()];
        assert_eq!(builds_mcp_server(&d, &other, ""), Some(false));

        for (name, text, want) in [
            (
                "pyproject.toml",
                "[project]\ndependencies = [\"mcp[cli]>=1.2\", \"httpx\"]\n",
                true,
            ),
            (
                "pyproject.toml",
                "[project]\nname = \"mcp-tools-docs\"\ndependencies = [\"httpx\"]\n",
                false,
            ),
            ("requirements.txt", "httpx==0.27\nfastmcp>=2.0\n", true),
            ("requirements.txt", "mcpx==1.0\n", false),
            (
                "Cargo.toml",
                "[dependencies]\nrmcp = { version = \"0.8\" }\n",
                true,
            ),
            ("Cargo.toml", "[dependencies]\nserde = \"1\"\n", false),
            (
                "go.mod",
                "module x\nrequire github.com/mark3labs/mcp-go v0.30.0\n",
                true,
            ),
        ] {
            let d = scope_dir("eco");
            std::fs::write(d.join(name), text).unwrap();
            assert_eq!(
                builds_mcp_server(&d, &["src/a.rs".to_string()], ""),
                Some(want),
                "{name}: {text}"
            );
        }
        let d = scope_dir("imports");
        for (line, want) in [
            ("+import { FastMCP } from \"fastmcp\";", true),
            (
                "+import { McpServer } from \"@modelcontextprotocol/sdk/server/mcp.js\";",
                true,
            ),
            ("+from mcp.server.fastmcp import FastMCP", true),
            ("+from fastmcp import FastMCP", true),
            ("+use rmcp::ServerHandler;", true),
            ("-import { FastMCP } from \"fastmcp\";", false),
            ("+// the mcp-server pack's rules", false),
            ("+const mcpServerUrl = cfg.mcp;", false),
        ] {
            let diff = format!("+++ b/src/a.ts\n{line}\n");
            assert_eq!(builds_mcp_server(&d, &[], &diff), Some(want), "{line}");
        }
        // A path that climbs out of the worktree names none of its directories.
        let up = vec!["../outside/src/a.ts".to_string()];
        assert_eq!(builds_mcp_server(&d, &up, ""), Some(false));
    }

    /// (core#1414) A manifest that exists and cannot be read leaves the fit unknown: nothing is
    /// removed on it.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_manifest_leaves_the_fit_unknown() {
        use std::os::unix::fs::PermissionsExt;
        let d = scope_dir("unreadable");
        let f = d.join("package.json");
        std::fs::write(&f, r#"{"dependencies":{"fastmcp":"3"}}"#).unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Root can read anything; the case only exists for an ordinary user.
        let readable = std::fs::read(&f).is_ok();
        let got = builds_mcp_server(&d, &["src/a.ts".to_string()], "");
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        if !readable {
            assert_eq!(got, None);
        }
    }
}
