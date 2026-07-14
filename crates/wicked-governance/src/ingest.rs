//! Ingest seam — where conformance rules come FROM, and the compliance-framework drop-in.
//!
//! Ported from the retired `conformance-ingest` + `conformance-frameworks`
//! (RET-BRAIN-DOMAIN-001). A [`SourceAdapter`] reads raw rule documents from some origin
//! (filesystem shipped; Confluence/SharePoint are declared stubs that fail LOUD, never silent-empty
//! — a silent empty reads as "no rules" and would fail governance OPEN). [`normalize_bundle`] turns
//! a raw doc into validated [`ConformanceRule`]s, failing loud on a missing load-bearing field
//! (never fabricated) and enforcing bundle-unique ids (INV-C3). [`ComplianceFramework`] +
//! [`FrameworkRegistry`] are the config-driven drop-in: the default is a no-op; real SOC2/PCI
//! resolvers register by name and are looked up on demand — the seam is tested once here.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::conformance::ConformanceRule;

/// A source of raw conformance-rule documents (filesystem, Confluence, SharePoint, …).
pub trait SourceAdapter {
    /// Stable adapter name (recorded as `provenance.source` on each ingested rule).
    fn name(&self) -> &str;
    /// Read the raw rule documents this source currently holds.
    fn fetch(&self) -> anyhow::Result<Vec<serde_json::Value>>;
}

/// Filesystem adapter — reads `*.json` rule bundles under a directory (the shipped connector).
pub struct FilesystemAdapter {
    root: PathBuf,
}

impl FilesystemAdapter {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl SourceAdapter for FilesystemAdapter {
    fn name(&self) -> &str {
        "filesystem"
    }

    fn fetch(&self) -> anyhow::Result<Vec<serde_json::Value>> {
        let entries = std::fs::read_dir(&self.root)
            .map_err(|e| anyhow::anyhow!("filesystem adapter: cannot read {:?}: {e}", self.root))?;
        // Propagate a mid-stream enumeration error (readdir can fault) rather than `.ok()`-dropping
        // it — a silent skip would truncate the rule set and fail governance OPEN (fail-loud contract).
        let mut paths: Vec<PathBuf> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| {
                anyhow::anyhow!("filesystem adapter: cannot enumerate {:?}: {e}", self.root)
            })?;
            let path = entry.path();
            // Case-INSENSITIVE extension: `Rules.JSON` on a case-insensitive FS is still a bundle
            // (cross-platform mandate — macOS/Windows produce mixed-case extensions).
            if path
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("json"))
            {
                paths.push(path);
            }
        }
        paths.sort(); // deterministic ingest order
        let mut docs = Vec::with_capacity(paths.len());
        for p in paths {
            let text = std::fs::read_to_string(&p)
                .map_err(|e| anyhow::anyhow!("filesystem adapter: cannot read {p:?}: {e}"))?;
            let doc: serde_json::Value = serde_json::from_str(&text)
                .map_err(|e| anyhow::anyhow!("filesystem adapter: {p:?} is not valid JSON: {e}"))?;
            docs.push(doc);
        }
        Ok(docs)
    }
}

/// A declared-but-unimplemented remote adapter (Confluence / SharePoint). Fails LOUD when fetched —
/// never silently returns empty, because a silent empty reads as "no rules" and would fail
/// governance OPEN. Real connectors replace this by implementing [`SourceAdapter`].
pub struct StubAdapter {
    name: &'static str,
}

impl StubAdapter {
    /// The Confluence connector stub.
    pub fn confluence() -> Self {
        Self { name: "confluence" }
    }
    /// The SharePoint connector stub.
    pub fn sharepoint() -> Self {
        Self { name: "sharepoint" }
    }
}

impl SourceAdapter for StubAdapter {
    fn name(&self) -> &str {
        self.name
    }

    fn fetch(&self) -> anyhow::Result<Vec<serde_json::Value>> {
        anyhow::bail!(
            "the {:?} source adapter is a declared stub (not implemented) — configure a real \
             connector or use the filesystem adapter; refusing to return an empty rule set",
            self.name
        )
    }
}

/// Load-bearing fields a raw rule MUST carry (fail loud rather than fabricate).
const REQUIRED_FIELDS: [&str; 4] = ["id", "rule_type", "statement", "severity"];

/// Normalize a raw doc into validated rules, stamping `source` as ingest provenance. Accepts either
/// a bundle (`{ "rules": [ … ] }`) or a single bare rule object. Fails LOUD on a missing required
/// field (never fabricated), on a parse failure, on an INV-C1/C2 violation, and on a duplicate id
/// within the bundle (INV-C3).
pub fn normalize_bundle(
    doc: &serde_json::Value,
    source: &str,
) -> anyhow::Result<Vec<ConformanceRule>> {
    let raw_rules: Vec<serde_json::Value> = match doc.get("rules") {
        Some(serde_json::Value::Array(a)) => a.clone(),
        Some(_) => anyhow::bail!("bundle from {source:?}: `rules` must be an array"),
        None => vec![doc.clone()], // a bare single rule
    };

    let mut rules = Vec::with_capacity(raw_rules.len());
    let mut seen: HashSet<String> = HashSet::new();
    for raw in &raw_rules {
        for field in REQUIRED_FIELDS {
            if raw.get(field).is_none() {
                anyhow::bail!(
                    "conformance rule from {source:?} is missing required `{field}` (never fabricated)"
                );
            }
        }
        let mut rule: ConformanceRule = serde_json::from_value(raw.clone()).map_err(|e| {
            anyhow::anyhow!("conformance rule from {source:?} failed to parse: {e}")
        })?;
        // Stamp the connector as `provenance.source` AUTHORITATIVELY (always overwrite): the adapter
        // that actually read the doc is the source of truth, so a doc cannot spoof a different source.
        // But `ref` (WHERE in the source) and `source_kinds` (the evidence kind) the ingest CANNOT
        // infer — the wire schema requires them, so a doc that omits them is a HARD failure, never
        // fabricated.
        rule.provenance.source = source.to_string();
        if rule.provenance.reference.is_none() || rule.provenance.source_kinds.is_empty() {
            anyhow::bail!(
                "conformance rule {:?} from {source:?} has incomplete provenance — the wire contract \
                 requires `ref` and a non-empty `source_kinds` (never fabricated)",
                rule.id
            );
        }
        rule.validate()?; // INV-C1 / INV-C2
        if !seen.insert(rule.id.clone()) {
            anyhow::bail!(
                "INV-C3: duplicate rule id {:?} within the {source:?} bundle",
                rule.id
            );
        }
        rules.push(rule);
    }
    Ok(rules)
}

/// Fetch + normalize every document an adapter holds, in the adapter's deterministic order. INV-C3
/// is enforced across the WHOLE run (not just per document): the same rule id appearing in two files
/// from one source fails loud rather than silently overwriting at register (both map to the same
/// `conformance_rule/<id>` symbol).
pub fn ingest_from(adapter: &dyn SourceAdapter) -> anyhow::Result<Vec<ConformanceRule>> {
    let mut all = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for doc in adapter.fetch()? {
        for rule in normalize_bundle(&doc, adapter.name())? {
            if !seen.insert(rule.id.clone()) {
                anyhow::bail!(
                    "INV-C3: duplicate rule id {:?} across documents from the {:?} source",
                    rule.id,
                    adapter.name()
                );
            }
            all.push(rule);
        }
    }
    Ok(all)
}

/// Resolves a rule's `compliance.control_id` against an external framework (SOC2 / PCI / …). A
/// drop-in seam: the default is a no-op; real resolvers register by name and are looked up on demand.
pub trait ComplianceFramework: Send + Sync {
    fn name(&self) -> &str;
    /// Resolve a control id to a human title / reference, or `None` if unknown. NEVER fabricates.
    fn resolve(&self, control_id: &str) -> Option<String>;
}

/// The default framework: recognizes nothing (a config-driven placeholder until a real SOC2/PCI
/// resolver drops in). Never fabricates a control title.
pub struct NoopFramework;

impl ComplianceFramework for NoopFramework {
    fn name(&self) -> &str {
        "noop"
    }
    fn resolve(&self, _control_id: &str) -> Option<String> {
        None
    }
}

/// A registry of compliance frameworks by name — the drop-in point. Real frameworks `register`
/// themselves; consumers `resolve` by (framework, control_id). An unregistered framework resolves
/// to `None` (the no-op behaviour), never an error and never a fabricated value.
#[derive(Default)]
pub struct FrameworkRegistry {
    frameworks: HashMap<String, Box<dyn ComplianceFramework>>,
}

impl FrameworkRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace) a framework, keyed by its `name()`.
    pub fn register(&mut self, framework: Box<dyn ComplianceFramework>) {
        self.frameworks
            .insert(framework.name().to_string(), framework);
    }

    /// Resolve `control_id` against the named framework, or `None` if the framework is unregistered
    /// or does not recognize the control.
    pub fn resolve(&self, framework: &str, control_id: &str) -> Option<String> {
        self.frameworks
            .get(framework)
            .and_then(|f| f.resolve(control_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A valid provenance block (the wire contract requires `ref` + `source_kinds`).
    fn prov() -> serde_json::Value {
        serde_json::json!({ "ref": "handbook#sec-1", "source_kinds": ["doc"] })
    }

    #[test]
    fn normalize_bundle_stamps_source_and_parses_rules() {
        let doc = serde_json::json!({
            "rules": [
                { "id": "PAT-001", "rule_type": "pattern", "statement": "no eval", "severity": "error", "confidence": 0.9,
                  "targets": { "language": "python" }, "provenance": prov() },
                { "id": "POL-001", "rule_type": "policy", "statement": "encrypt at rest", "severity": "critical",
                  "confidence": 1.0, "compliance": { "framework": "soc2", "control_id": "CC6.1" }, "provenance": prov() }
            ]
        });
        let rules = normalize_bundle(&doc, "filesystem").unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].provenance.source, "filesystem", "source stamped");
        assert_eq!(rules[0].provenance.source_kinds, vec!["doc".to_string()]);
        assert_eq!(rules[0].targets.language.as_deref(), Some("python"));
        assert_eq!(
            rules[1].compliance.as_ref().unwrap().control_id,
            "CC6.1",
            "nested compliance parsed"
        );
    }

    #[test]
    fn normalize_fails_loud_on_missing_field() {
        // no `statement` — must fail, never fabricate.
        let doc = serde_json::json!({ "rules": [
            { "id": "PAT-001", "rule_type": "pattern", "severity": "error", "confidence": 0.5, "provenance": prov() }
        ]});
        let err = normalize_bundle(&doc, "filesystem")
            .unwrap_err()
            .to_string();
        assert!(err.contains("statement"), "got: {err}");
    }

    #[test]
    fn normalize_fails_loud_on_incomplete_provenance() {
        // All rule fields present, but provenance omits ref/source_kinds — schema-required, never fabricated.
        let doc = serde_json::json!({ "rules": [
            { "id": "PAT-001", "rule_type": "pattern", "statement": "s", "severity": "info", "confidence": 0.5 }
        ]});
        let err = normalize_bundle(&doc, "filesystem")
            .unwrap_err()
            .to_string();
        assert!(err.contains("provenance"), "got: {err}");
    }

    #[test]
    fn normalize_accepts_a_bare_single_rule() {
        // A doc with NO `rules` key is a single bare rule (the explicit second branch).
        let doc = serde_json::json!(
            { "id": "PAT-001", "rule_type": "pattern", "statement": "s", "severity": "info", "confidence": 0.5, "provenance": prov() }
        );
        let rules = normalize_bundle(&doc, "confluence").unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, "PAT-001");
        assert_eq!(rules[0].provenance.source, "confluence");
    }

    #[test]
    fn normalize_enforces_inv_c3_unique_ids_within_a_bundle() {
        let doc = serde_json::json!({ "rules": [
            { "id": "PAT-001", "rule_type": "pattern", "statement": "a", "severity": "info", "confidence": 0.5, "provenance": prov() },
            { "id": "PAT-001", "rule_type": "pattern", "statement": "b", "severity": "info", "confidence": 0.5, "provenance": prov() }
        ]});
        let err = normalize_bundle(&doc, "filesystem")
            .unwrap_err()
            .to_string();
        assert!(err.contains("INV-C3"), "got: {err}");
    }

    #[test]
    fn ingest_enforces_inv_c3_across_documents() {
        // The SAME id in two files from one adapter must fail loud (would silently overwrite at register).
        struct TwoDocs;
        impl SourceAdapter for TwoDocs {
            fn name(&self) -> &str {
                "twodocs"
            }
            fn fetch(&self) -> anyhow::Result<Vec<serde_json::Value>> {
                let r = serde_json::json!({ "rules": [
                    { "id": "PAT-001", "rule_type": "pattern", "statement": "s", "severity": "info", "confidence": 0.5,
                      "provenance": { "ref": "x", "source_kinds": ["doc"] } }
                ]});
                Ok(vec![r.clone(), r])
            }
        }
        let err = ingest_from(&TwoDocs).unwrap_err().to_string();
        assert!(
            err.contains("INV-C3"),
            "cross-document dup must fail: {err}"
        );
    }

    #[test]
    fn stub_adapter_fails_loud_never_empty() {
        for stub in [StubAdapter::confluence(), StubAdapter::sharepoint()] {
            let err = stub.fetch().unwrap_err().to_string();
            assert!(
                err.contains("stub"),
                "a stub must fail loud, not return empty: {err}"
            );
        }
    }

    #[test]
    fn framework_registry_defaults_to_noop_then_resolves_registered() {
        struct Soc2;
        impl ComplianceFramework for Soc2 {
            fn name(&self) -> &str {
                "soc2"
            }
            fn resolve(&self, control_id: &str) -> Option<String> {
                (control_id == "CC6.1").then(|| "Logical Access".to_string())
            }
        }
        let mut reg = FrameworkRegistry::new();
        // Unregistered → no-op None (never an error, never fabricated).
        assert_eq!(reg.resolve("soc2", "CC6.1"), None);
        reg.register(Box::new(NoopFramework));
        reg.register(Box::new(Soc2));
        assert_eq!(
            reg.resolve("soc2", "CC6.1").as_deref(),
            Some("Logical Access")
        );
        assert_eq!(reg.resolve("soc2", "CC9.9"), None, "unknown control → None");
        assert_eq!(
            reg.resolve("pci", "1.1"),
            None,
            "unregistered framework → None"
        );
    }

    #[test]
    fn filesystem_adapter_reads_json_bundles_in_order() {
        // Per-process unique dir so concurrent test binaries don't collide on a fixed path.
        let dir = std::env::temp_dir().join(format!("wicked-gov-ingest-fs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("a.json"),
            r#"{"rules":[{"id":"PAT-001","rule_type":"pattern","statement":"s","severity":"info","confidence":0.5,"provenance":{"ref":"x","source_kinds":["doc"]}}]}"#,
        )
        .unwrap();
        std::fs::write(dir.join("ignore.txt"), "not json").unwrap();
        let rules = ingest_from(&FilesystemAdapter::new(&dir)).unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, "PAT-001");
        assert_eq!(rules[0].provenance.source, "filesystem");
    }
}
