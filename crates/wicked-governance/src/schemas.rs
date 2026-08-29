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

/// Every schema in the bundle as `(file_name, contents)` — lets callers (tests, future graph
/// registration per schemas/README.md TODO) iterate without hardcoding the roster twice.
pub const SCHEMA_BUNDLE: [(&str, &str); 4] = [
    ("conformance-rules.schema.json", CONFORMANCE_RULES_SCHEMA),
    ("domain-model.schema.json", DOMAIN_MODEL_SCHEMA),
    ("coverage.schema.json", COVERAGE_SCHEMA),
    ("vocabulary.schema.json", VOCABULARY_SCHEMA),
];

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

    /// Each schema's `metadata.schema_version` const (the version a DOCUMENT carries) must agree
    /// with its `$id` version segment — the two spellings of the per-schema contract version.
    #[test]
    fn schema_version_consts_match_their_ids() {
        for (name, raw) in [
            ("conformance-rules.schema.json", CONFORMANCE_RULES_SCHEMA),
            ("domain-model.schema.json", DOMAIN_MODEL_SCHEMA),
        ] {
            let schema = parsed(name, raw);
            let id_ver = id_version(name, &schema);
            let const_ver = schema["properties"]["metadata"]["properties"]["schema_version"]
                ["const"]
                .as_str()
                .unwrap_or_else(|| panic!("{name} has no metadata.schema_version const"));
            assert_eq!(
                const_ver, id_ver,
                "{name}: metadata.schema_version const {const_ver:?} != $id segment {id_ver:?}"
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
}
