//! RuleSet grouping — one `NodeKind::RuleSet` parent per doctrine domain (AW-13 / arch-R9).
//!
//! Ungrouped rules are second-class on every read surface: estate's `RulesInventory` lists
//! `RuleSet` engine nodes and counts a rule as *grouped* only when a native `Contains` edge
//! reaches it, and DES-OUTGOV-001 §3 carries Domain→RuleSet as the wiki's grouping unit. This
//! module mints that parent: a doc's frontmatter `domain:` (the [`crate::MarkdownAdapter`]
//! convention) selects the RuleSet every rule in the doc lands under.
//!
//! Vocabulary is estate-NATIVE on both halves — `NodeKind::RuleSet` for the parent node (the
//! same kind the rules-engine extractors mint, so `RulesInventory` sees doctrine RuleSets with
//! zero reader change) and `EdgeKind::Contains` for the membership edge (source = the RuleSet
//! container, target = the contained rule — the extractor `Contains` convention). The synthetic
//! symbol namespace is [`RULE_SET`] (`rule_set/<domain>`), so re-ingest UPSERTS the same node and
//! `upsert_edges` dedups the same membership edge: grouping is as idempotent as the rule ingest
//! it rides.
//!
//! Both endpoint nodes exist before the edge lands (the caller registers rules first; this module
//! upserts the RuleSet node in the same batch as its edges), so `Contains` membership never
//! dangles into estate's `prune_dangling_edges`.

use wicked_apps_core::{
    synthetic_symbol, Edge, EdgeKind, GraphStore, Language, Location, Metadata, Node, NodeKind,
    ResolutionTier, Span, SYMBOL_SCHEME,
};

use crate::conformance::CONFORMANCE_RULE;

/// Symbol-namespace prefix for doctrine RuleSet symbols (`rule_set/<domain>`; the NODE kind is
/// the native [`NodeKind::RuleSet`]).
pub const RULE_SET: &str = "rule_set";
/// The concrete `resolved_by` id on every membership edge this module emits.
const RULESET_RESOLVED_BY: &str = "wicked-governance-ruleset";

/// One doctrine domain's membership: the RuleSet parent plus the rule ids it contains. Derived
/// from doc frontmatter (`domain:` selects the parent) by [`crate::MarkdownAdapter::groupings`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleSetGrouping {
    /// The doctrine domain — RuleSet node name and synthetic-symbol id (`rule_set/<domain>`).
    pub domain: String,
    /// The `PAT-`/`POL-` ids of the rules this RuleSet contains.
    pub rule_ids: Vec<String>,
}

/// The stable synthetic symbol of one doctrine RuleSet.
pub fn rule_set_symbol(domain: &str) -> wicked_apps_core::SymbolId {
    synthetic_symbol(RULE_SET, domain)
}

/// Persist the RuleSet parents + `Contains` membership edges for `groupings`. Idempotent: the
/// node upserts on its stable synthetic symbol, the edges dedup on the edge key. Returns
/// `(rule_sets, membership_edges)` written. Call AFTER the member rules are registered — the
/// membership edge's target must exist so it can never dangle.
///
/// Fail-loud contract matches the rest of the ingest path: an empty `domain` or an empty
/// `rule_ids` list is a hard error (a parentless grouping row would read as "grouped" while
/// grouping nothing).
pub fn register_rule_sets(
    store: &mut dyn GraphStore,
    groupings: &[RuleSetGrouping],
) -> anyhow::Result<(usize, usize)> {
    if groupings.is_empty() {
        return Ok((0, 0));
    }
    let mut nodes: Vec<Node> = Vec::with_capacity(groupings.len());
    let mut edges: Vec<Edge> = Vec::new();
    for grouping in groupings {
        if grouping.domain.trim().is_empty() {
            anyhow::bail!("RuleSet grouping with an empty domain — refusing (fail-loud)");
        }
        if grouping.rule_ids.is_empty() {
            anyhow::bail!(
                "RuleSet grouping {:?} contains no rules — refusing an empty grouping (fail-loud)",
                grouping.domain
            );
        }
        let symbol = rule_set_symbol(&grouping.domain);
        let mut node = Node::new(
            symbol.clone(),
            NodeKind::RuleSet,
            grouping.domain.clone(),
            Language::new(SYMBOL_SCHEME),
            Location::new(format!("{RULE_SET}/{}", grouping.domain), Span::ZERO),
        );
        let mut meta = Metadata::new();
        meta.insert(
            "domain".into(),
            serde_json::Value::String(grouping.domain.clone()),
        );
        node.metadata = meta;
        nodes.push(node);
        for rule_id in &grouping.rule_ids {
            edges.push(Edge::new(
                symbol.clone(),
                synthetic_symbol(CONFORMANCE_RULE, rule_id),
                EdgeKind::Contains,
                ResolutionTier::Parsed,
                RULESET_RESOLVED_BY,
            ));
        }
    }
    // Belt-and-braces: membership is Contains, but every governance batch goes through the
    // vocabulary pin so a future edit here cannot re-introduce a stringly spelling (AW-19).
    crate::edge_vocab::assert_edge_vocabulary(&edges)?;
    store.begin_batch()?;
    store.upsert_nodes(&nodes)?;
    store.upsert_edges(&edges)?;
    store.commit_batch()?;
    Ok((nodes.len(), edges.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::{register_rule, ConfSeverity, ConformanceRule, RuleType, Targets};
    use crate::RuleProvenance;
    use wicked_apps_core::{open_store, GraphRead};
    use wicked_estate_core::{Direction, SymbolQuery};

    fn rule(id: &str, ty: RuleType) -> ConformanceRule {
        ConformanceRule {
            id: id.to_string(),
            rule_type: ty,
            statement: format!("statement for {id}"),
            severity: ConfSeverity::Error,
            confidence: 1.0,
            targets: Targets::default(),
            symbol_ref: None,
            compliance: None,
            provenance: RuleProvenance::default(),
            retired: false,
            ..Default::default()
        }
    }

    #[test]
    fn rule_sets_round_trip_with_contains_membership() {
        let mut store = open_store(Some(":memory:")).unwrap();
        for r in [
            rule("PAT-901", RuleType::Pattern),
            rule("POL-902", RuleType::Policy),
        ] {
            register_rule(&mut store, &r).unwrap();
        }
        let groupings = [RuleSetGrouping {
            domain: "agent-behavior".to_string(),
            rule_ids: vec!["PAT-901".to_string(), "POL-902".to_string()],
        }];
        let (sets, edges) = register_rule_sets(&mut store, &groupings).unwrap();
        assert_eq!((sets, edges), (1, 2));

        // The parent is a NATIVE RuleSet node (what RulesInventory lists).
        let sets = store
            .find_symbols(&SymbolQuery {
                kinds: vec![NodeKind::RuleSet],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].name, "agent-behavior");
        assert_eq!(sets[0].symbol, rule_set_symbol("agent-behavior"));

        // Membership = native Contains edges, RuleSet → rule (both directions reachable).
        let out = store
            .neighbors(&rule_set_symbol("agent-behavior"), Direction::Dependencies)
            .unwrap();
        let contains: Vec<_> = out
            .iter()
            .filter(|e| e.kind == EdgeKind::Contains)
            .collect();
        assert_eq!(contains.len(), 2);
        assert!(contains
            .iter()
            .any(|e| e.target == synthetic_symbol(CONFORMANCE_RULE, "PAT-901")));

        // Idempotent: a re-run upserts the same node + dedups the same edges.
        register_rule_sets(&mut store, &groupings).unwrap();
        let out2 = store
            .neighbors(&rule_set_symbol("agent-behavior"), Direction::Dependencies)
            .unwrap();
        assert_eq!(
            out2.iter().filter(|e| e.kind == EdgeKind::Contains).count(),
            2,
            "re-registering must not duplicate membership edges"
        );
    }

    #[test]
    fn empty_grouping_is_refused() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let err = register_rule_sets(
            &mut store,
            &[RuleSetGrouping {
                domain: "event-grammar".to_string(),
                rule_ids: vec![],
            }],
        )
        .expect_err("an empty grouping must fail loud");
        assert!(err.to_string().contains("no rules"));
    }
}
