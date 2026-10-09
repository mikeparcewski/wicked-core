//! (core#804) The guidance steering packs the engine's shipped workflows cite, seeded into the
//! store at boot so `rules.recall` finds them without an operator `rules ingest`.
//!
//! The `mcp-server` workflow's design and review phases are told to check their work against
//! MCPS-1001..1007, and every governed review loop against RVWL-1001..1004. Both packs were opt-in
//! (`wicked-core rules ingest governance/packs/<pack>` or Steering → Import), and nothing at launch
//! said so: on the rig the store held 14 rules and none of them, the design unit's recall came back
//! empty, and the seat invented meanings for rule ids it could not read (run `c3aa0bfb`).
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

/// One shipped pack: its directory name under `governance/packs/` and its docs, root-relative.
struct ShippedPack {
    name: &'static str,
    docs: &'static [(&'static str, &'static [u8])],
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
/// engine denies and stays an operator's explicit ingest.
const SHIPPED_PACKS: &[ShippedPack] = &[
    ShippedPack {
        name: "mcp-server",
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
        docs: &[
            pack_doc!("review-loop", "review-loop-bounded-rework.md"),
            pack_doc!("review-loop", "review-loop-foreground-evidence.md"),
            pack_doc!("review-loop", "review-loop-one-slice-intent.md"),
            pack_doc!("review-loop", "review-loop-stateful-verdicts.md"),
        ],
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
        for want in ["MCPS-1001", "MCPS-1007", "RVWL-1001", "RVWL-1004"] {
            assert!(ids.contains(&want), "{want} ships: {ids:?}");
        }
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
}
