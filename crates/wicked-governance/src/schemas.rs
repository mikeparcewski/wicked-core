//! The governance schema bundle — embedded from `schemas/`, the LIVE OWNER copies.
//!
//! Re-homed from the retired wicked-brain repo (AW-2 / arch-R10): the crate that enforces these
//! contracts embeds them, so the schemas can never drift from the code that claims to mirror them.
//! See `schemas/README.md` for ownership, version semantics (bundle `VERSION` vs per-schema
//! contract version), and the cross-repo sync story (garden's vendor pin test byte-compares
//! against `schemas/`).

/// PRESCRIPTIVE conformance rules applied TO code — the wire contract [`crate::ConformanceRule`]
/// mirrors (formerly "the retired conformance-rules schema"; this copy is now the live one).
pub const CONFORMANCE_RULES_SCHEMA: &str = include_str!("../schemas/conformance-rules.schema.json");
/// DESCRIPTIVE domain model mined FROM code — the contract garden STEERS on.
pub const DOMAIN_MODEL_SCHEMA: &str = include_str!("../schemas/domain-model.schema.json");
/// Front-half coverage report wire shape (`coverage-report.json`).
pub const COVERAGE_SCHEMA: &str = include_str!("../schemas/coverage.schema.json");
/// Domain vocabulary spine.
pub const VOCABULARY_SCHEMA: &str = include_str!("../schemas/vocabulary.schema.json");

const BUNDLE_VERSION_RAW: &str = include_str!("../schemas/VERSION");

/// Semver of the whole schema BUNDLE (the `schemas/VERSION` file, trimmed). Independent of each
/// schema's own contract version (`$id` segment / `metadata.schema_version` const) — the schemas
/// document that independence themselves.
pub fn schema_bundle_version() -> &'static str {
    BUNDLE_VERSION_RAW.trim()
}

/// Every schema in the bundle as `(file_name, contents)` — lets callers (tests, graph
/// registration below) iterate without hardcoding the roster twice.
pub const SCHEMA_BUNDLE: [(&str, &str); 4] = [
    ("conformance-rules.schema.json", CONFORMANCE_RULES_SCHEMA),
    ("domain-model.schema.json", DOMAIN_MODEL_SCHEMA),
    ("coverage.schema.json", COVERAGE_SCHEMA),
    ("vocabulary.schema.json", VOCABULARY_SCHEMA),
];

/// Node kind + synthetic-symbol namespace for schema-document nodes (the schemas/README.md AW-3
/// seam). `NodeKind::Other(GOVERNANCE_SCHEMA)` for now — a first-class const in wicked-apps-core
/// (beside `POLICY`/`CONFORMANCE_CLAIM`) belongs to the apps-core consts pass (AW-12/AW-19).
pub const GOVERNANCE_SCHEMA: &str = "governance_schema";

/// Register the schema bundle on the graph: one node per schema file, **keyed by `$id`**
/// (version-addressed — a contract bump mints a NEW node, so a `Rule` can point at the exact
/// contract it was validated under), carrying the per-schema contract version + the bundle
/// version. Idempotent (same `$id` upserts the same synthetic symbol). Fails loud if an owner
/// copy is corrupt — a governance store must never carry a schema node it cannot re-derive.
pub fn register_schema_nodes(
    store: &mut dyn wicked_apps_core::GraphStore,
) -> anyhow::Result<usize> {
    use wicked_apps_core::{
        synthetic_symbol, Language, Location, Node, NodeKind, Span, SYMBOL_SCHEME,
    };
    let mut nodes = Vec::with_capacity(SCHEMA_BUNDLE.len());
    for (file, raw) in SCHEMA_BUNDLE {
        let schema: serde_json::Value = serde_json::from_str(raw).map_err(|e| {
            anyhow::anyhow!("schema {file} is not valid JSON (owner copy corrupt): {e}")
        })?;
        let id = schema["$id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("schema {file} has no string $id"))?;
        let contract_version = id
            .rsplit('/')
            .next()
            .filter(|seg| seg.split('.').count() == 3)
            .ok_or_else(|| {
                anyhow::anyhow!("schema {file} $id {id:?} does not end in a /x.y.z segment")
            })?;
        let mut node = Node::new(
            synthetic_symbol(GOVERNANCE_SCHEMA, id),
            NodeKind::Other(GOVERNANCE_SCHEMA.to_string()),
            id.to_string(),
            Language::new(SYMBOL_SCHEME),
            Location::new(format!("{GOVERNANCE_SCHEMA}/{file}"), Span::ZERO),
        );
        node.metadata.insert("file".into(), file.into());
        node.metadata.insert("schema_id".into(), id.into());
        node.metadata
            .insert("contract_version".into(), contract_version.into());
        node.metadata
            .insert("bundle_version".into(), schema_bundle_version().into());
        if let Some(title) = schema["title"].as_str() {
            node.metadata.insert("title".into(), title.into());
        }
        nodes.push(node);
    }
    store.begin_batch()?;
    store.upsert_nodes(&nodes)?;
    store.commit_batch()?;
    Ok(nodes.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn parsed(name: &str, raw: &str) -> Value {
        serde_json::from_str(raw)
            .unwrap_or_else(|e| panic!("{name} is not valid JSON (owner copy corrupt): {e}"))
    }

    /// `$id` ends in `/<semver>`; return that segment.
    fn id_version(name: &str, schema: &Value) -> String {
        let id = schema["$id"]
            .as_str()
            .unwrap_or_else(|| panic!("{name} has no string $id"));
        id.rsplit('/')
            .next()
            .filter(|seg| seg.split('.').count() == 3)
            .unwrap_or_else(|| panic!("{name} $id {id:?} does not end in a /x.y.z segment"))
            .to_string()
    }

    #[test]
    fn every_schema_parses_and_carries_a_versioned_id() {
        for (name, raw) in SCHEMA_BUNDLE {
            let schema = parsed(name, raw);
            let ver = id_version(name, &schema);
            assert!(
                ver.split('.').all(|p| p.parse::<u64>().is_ok()),
                "{name} $id version segment {ver:?} is not numeric semver"
            );
        }
    }

    #[test]
    fn bundle_version_is_semver() {
        let v = schema_bundle_version();
        assert_eq!(
            v.split('.').count(),
            3,
            "schemas/VERSION {v:?} is not x.y.z"
        );
        assert!(
            v.split('.').all(|p| p.parse::<u64>().is_ok()),
            "schemas/VERSION {v:?} has a non-numeric segment"
        );
    }

    /// Each schema's `metadata.schema_version` (the version a DOCUMENT carries) must agree with
    /// its `$id` version segment — the two spellings of the per-schema contract version. A schema
    /// pins it as a `const` while it has one accepted version; an additively-evolved schema (the
    /// conformance-rules 1.1.0 STEERING bump) widens it to an `enum` of every still-valid version,
    /// whose NEWEST entry must be the `$id` segment.
    #[test]
    fn schema_version_consts_match_their_ids() {
        for (name, raw) in [
            ("conformance-rules.schema.json", CONFORMANCE_RULES_SCHEMA),
            ("domain-model.schema.json", DOMAIN_MODEL_SCHEMA),
        ] {
            let schema = parsed(name, raw);
            let id_ver = id_version(name, &schema);
            let sv = &schema["properties"]["metadata"]["properties"]["schema_version"];
            let newest = sv["const"]
                .as_str()
                .map(str::to_string)
                .or_else(|| {
                    sv["enum"].as_array().map(|versions| {
                        versions
                            .iter()
                            .map(|v| v.as_str().expect("enum member is a string").to_string())
                            .max_by(|a, b| {
                                let key = |s: &str| -> Vec<u64> {
                                    s.split('.').map(|p| p.parse().expect("semver")).collect()
                                };
                                key(a).cmp(&key(b))
                            })
                            .expect("non-empty schema_version enum")
                    })
                })
                .unwrap_or_else(|| panic!("{name} has no metadata.schema_version const or enum"));
            assert_eq!(
                newest, id_ver,
                "{name}: newest accepted metadata.schema_version {newest:?} != $id segment {id_ver:?}"
            );
        }
    }

    /// The crate's fail-closed INV-C4 vocabulary is now VALIDATED against the schema it claims to
    /// mirror: `VALID_SOURCE_KINDS` == the shared `$defs/provenance.source_kinds` enum in BOTH the
    /// conformance-rules and domain-model schemas. This is what "live owner" means — the enforcing
    /// constant and the schema enum can no longer drift silently.
    #[test]
    fn valid_source_kinds_matches_the_schema_enum() {
        for (name, raw) in [
            ("conformance-rules.schema.json", CONFORMANCE_RULES_SCHEMA),
            ("domain-model.schema.json", DOMAIN_MODEL_SCHEMA),
        ] {
            let schema = parsed(name, raw);
            let enum_vals: Vec<&str> = schema["$defs"]["provenance"]["properties"]["source_kinds"]
                ["items"]["enum"]
                .as_array()
                .unwrap_or_else(|| panic!("{name} has no $defs/provenance source_kinds enum"))
                .iter()
                .map(|v| v.as_str().expect("enum member is a string"))
                .collect();
            assert_eq!(
                enum_vals,
                crate::conformance::VALID_SOURCE_KINDS,
                "{name} provenance.source_kinds enum drifted from INV-C4's VALID_SOURCE_KINDS"
            );
        }
    }

    /// The domain-model builder emits `schema_version` documents pinned to the schema const —
    /// assert the owner copy still pins the version `build_domain_model` callers pass ("1.0.0").
    #[test]
    fn domain_model_contract_version_is_1_0_0() {
        let schema = parsed("domain-model.schema.json", DOMAIN_MODEL_SCHEMA);
        assert_eq!(
            schema["properties"]["metadata"]["properties"]["schema_version"]["const"],
            "1.0.0",
            "domain-model contract version moved — update domain_model.rs callers + this pin together"
        );
    }

    /// The schemas/README.md AW-3 seam: one node per schema file, keyed by `$id`, carrying the
    /// contract version + bundle version — idempotent under re-registration.
    #[test]
    fn register_schema_nodes_mints_one_node_per_schema_keyed_by_id() {
        use wicked_apps_core::{synthetic_symbol, GraphRead, NodeKind, SqliteStore};
        let mut store = SqliteStore::in_memory().unwrap();
        assert_eq!(register_schema_nodes(&mut store).unwrap(), 4);
        // Idempotent: a re-run upserts the same $id-keyed symbols, never duplicates.
        assert_eq!(register_schema_nodes(&mut store).unwrap(), 4);
        let query = wicked_estate_core::SymbolQuery {
            kinds: vec![NodeKind::Other(GOVERNANCE_SCHEMA.to_string())],
            ..Default::default()
        };
        let nodes = store.find_symbols(&query).unwrap();
        assert_eq!(nodes.len(), 4, "one node per schema file, no dupes");
        for node in &nodes {
            let id = node.metadata["schema_id"].as_str().expect("schema_id");
            assert_eq!(
                node.symbol,
                synthetic_symbol(GOVERNANCE_SCHEMA, id),
                "keyed by $id"
            );
            assert_eq!(
                node.metadata["bundle_version"].as_str(),
                Some(schema_bundle_version())
            );
            let cv = node.metadata["contract_version"]
                .as_str()
                .expect("contract_version");
            assert!(
                id.ends_with(cv),
                "$id {id:?} carries its contract version {cv:?}"
            );
        }
    }
}
