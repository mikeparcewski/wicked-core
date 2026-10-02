//! EDITOR GRANTS — what an artifact editor plugin may do, decided on the steering engine
//! (DES-artifact-editor-plugins §6.2, slice EP-K1).
//!
//! # Where this sits
//!
//! Studio hosts an editor plugin in a sandboxed frame and enforces, on every request, the grant
//! set crew answers at `GET /api/v1/editors/:id/grants?project=`. Crew asks core through the
//! `evaluateEditorGrants` binding; [`evaluate_editor_grants`] answers. Nothing is recorded: a grant
//! decision is a read, re-derived on every settings or rule change.
//!
//! # The decision, per permission
//!
//! 1. **Policy**: `select_any` over the subject tokens ([`subject_tokens`]: `editor`,
//!    `editor:<id>`, `editor:<id>/<permission>`, `editor-perm:<permission>`, `project:<id>` when
//!    the check is for a project, and `editor-origin:first-party` for a first-party editor), then
//!    `decide`. **Deny dominates**: a denying rule wins over the approvals ledger and every default.
//! 2. **The approvals ledger**: the [`GRANTS_LEDGER`] rule's `excludes`, read whether that rule is
//!    active or retired. Its tokens are [`grant_token`]: `editor:<id>@<version>#<sha256>/<permission>`,
//!    the FULL 64-hex sha256 of the entry file and the editor's version, so a new version or a
//!    changed entry is a different token and asks again (rev 2, review S9).
//! 3. **Defaults**: an `allow` rule that fired (the shipped `EDITOR-BUILTIN*` and
//!    `EDITOR-OPEN-DEFAULTS` posture rules) allows.
//! 4. Anything else **asks**.
//!
//! First-party means a `wicked-*` id AND an entry hash the caller vouches is the one studio ships
//! (`first_party: true`): a forged `wicked-` id with another hash gets no `editor-origin:first-party`
//! token, so the first-party defaults never fire for it.
//!
//! # Trust boundary
//!
//! The request's identity (`editorId`, `version`, `sha256`, `firstParty`) is CREW's measurement,
//! never the plugin's claim: crew hashes the entry file it serves from the installed bundle (the
//! bytes the frame will load, refused when they do not match the manifest), and sets `firstParty`
//! only when that hash is the one studio's bundle ships for the `wicked-*` id. The binding is
//! daemon-internal (napi); no plugin reaches it. Core holds no list of studio's hashes: they change
//! with every studio release, so the vouching lives where the files are.
//!
//! Like `mcp-defaults`, the boot seed carries the effect-bearing posture rules only; the recall-only
//! doctrine twins (ED-1 … ED-4) ride `rules ingest governance/packs/editor-defaults`.
//!
//! An unseeded store is a guard error (fail closed): without the posture rules a first-party editor
//! would ask for everything, and without the ledger nothing could be approved.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use wicked_apps_core::{open_store_ro, synthetic_symbol, GraphRead};

/// The pack rule that is the approvals ledger: its `excludes` lists the approved grant tokens.
pub(crate) const GRANTS_LEDGER: &str = "EDITOR-GRANTS";

/// The permissions an editor can ask for (§6.1). Anything else is a bad request.
pub const EDITOR_PERMISSIONS: [&str; 11] = [
    "artifact.read",
    "artifact.write",
    "selection.chip",
    "composer.draft",
    "checks.read",
    "checks.contribute",
    "sources.read",
    "media.read",
    "artifact.export",
    "ui.fullscreen",
    "network.media",
];

/// The evaluation scope the grant claims are judged under.
const SCOPE: &str = "wicked-editor/grants";

/// The policy pack the defaults ship in, embedded so the seed and `rules ingest` read one source.
const EDITOR_DEFAULTS_RULES: &str =
    include_str!("../governance/packs/editor-defaults/rules/editor-defaults.json");

/// One grants question: which of `permissions` may this editor use (for this project)?
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EditorGrantRequest {
    /// The editor's id (`wicked-page`, `acme-notes`).
    pub editor_id: String,
    /// The editor's version, as its manifest states it.
    pub version: String,
    /// The full lowercase 64-hex sha256 of the editor's entry file.
    pub sha256: String,
    /// The permissions to decide (the manifest's list, or one permission).
    pub permissions: Vec<String>,
    /// The project the editor is opened in, if any.
    #[serde(default)]
    pub project: Option<String>,
    /// The caller vouches the entry hash is the one shipped in studio's bundle for this id.
    #[serde(default)]
    pub first_party: bool,
}

/// One permission's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorGrant {
    pub permission: String,
    /// `allow` | `ask` | `deny`.
    pub decision: &'static str,
    /// The rules behind the answer: the denying rules, the allow rules that fired, or
    /// [`GRANTS_LEDGER`] for an approval.
    pub rule_ids: Vec<String>,
    /// The ledger token that approves this permission — what the operator's "allow" adds.
    pub token: String,
}

/// The whole answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorGrants {
    pub editor_id: String,
    pub version: String,
    pub sha256: String,
    pub project: Option<String>,
    pub first_party: bool,
    pub grants: Vec<EditorGrant>,
}

/// Why no answer could be given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorGrantError {
    BadRequest(String),
    GuardError(String),
}

impl std::fmt::Display for EditorGrantError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditorGrantError::BadRequest(m) => write!(f, "bad_request: {m}"),
            EditorGrantError::GuardError(m) => write!(f, "guard_error: {m}"),
        }
    }
}

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn valid_sha256(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
}

/// The ledger token that approves `permission` for exactly this editor build.
pub fn grant_token(editor_id: &str, version: &str, sha256: &str, permission: &str) -> String {
    format!("editor:{editor_id}@{version}#{sha256}/{permission}")
}

/// Whether the request's id may carry the first-party origin token.
fn first_party(req: &EditorGrantRequest) -> bool {
    req.first_party && req.editor_id.starts_with("wicked-")
}

/// The subject tokens one permission check selects rules by (§6.2).
fn subject_tokens(req: &EditorGrantRequest, permission: &str) -> Vec<String> {
    let mut t = vec![
        "editor".to_string(),
        format!("editor:{}", req.editor_id),
        format!("editor:{}/{permission}", req.editor_id),
        format!("editor-perm:{permission}"),
    ];
    if let Some(p) = &req.project {
        t.push(format!("project:{p}"));
    }
    if first_party(req) {
        t.push("editor-origin:first-party".to_string());
    }
    t
}

/// The canonical context a rule's trigger reads. Keys are written in sorted order so a trigger
/// spanning adjacent fields reads the same whatever the JSON map ordering.
fn context(req: &EditorGrantRequest, permission: &str) -> Value {
    serde_json::json!({
        "editor": {
            "first_party": first_party(req),
            "id": req.editor_id,
            "permission": permission,
            "sha256": req.sha256,
            "version": req.version,
        },
        "project": req.project,
        "scope": SCOPE,
        "tool": format!("editor:{}/{permission}", req.editor_id),
        "work": format!("editor:{}/{permission}", req.editor_id),
    })
}

fn validate(req: &EditorGrantRequest) -> Result<(), EditorGrantError> {
    let bad = |m: String| Err(EditorGrantError::BadRequest(m));
    if !valid_name(&req.editor_id) {
        return bad(format!(
            "editorId must be a non-empty name of [A-Za-z0-9._-] (got {:?})",
            req.editor_id
        ));
    }
    if !valid_name(&req.version) {
        return bad(format!(
            "version must be a non-empty name of [A-Za-z0-9._-] (got {:?})",
            req.version
        ));
    }
    if !valid_sha256(&req.sha256) {
        return bad(
            "sha256 must be the full 64-hex lowercase sha256 of the entry file (a prefix is not \
             a pin)"
                .to_string(),
        );
    }
    if req.permissions.is_empty() {
        return bad("permissions is empty".to_string());
    }
    for p in &req.permissions {
        if !EDITOR_PERMISSIONS.contains(&p.as_str()) {
            return bad(format!("unknown permission {p:?}"));
        }
    }
    if let Some(p) = &req.project {
        if !valid_name(p) {
            return bad(format!(
                "project must be a name of [A-Za-z0-9._-] (got {p:?})"
            ));
        }
    }
    Ok(())
}

/// The store's row for rule `id`, active or retired, or `None`.
fn rule_row(
    store: &dyn GraphRead,
    id: &str,
) -> Result<Option<wicked_governance::ConformanceRule>, EditorGrantError> {
    use wicked_apps_core::FromNode;
    let symbol = synthetic_symbol(wicked_governance::CONFORMANCE_RULE, id);
    store
        .get_node(&symbol)
        .map_err(|e| EditorGrantError::GuardError(format!("rule read failed: {e}")))?
        .map(|n| {
            wicked_governance::ConformanceRule::from_node(&n)
                .map_err(|e| EditorGrantError::GuardError(format!("rule {id} unreadable: {e}")))
        })
        .transpose()
}

/// Decide `req` against the rules in `store`. Pure: nothing is written.
pub fn evaluate_editor_grants(
    store: &dyn GraphRead,
    req: &EditorGrantRequest,
    evaluated_at: i64,
) -> Result<EditorGrants, EditorGrantError> {
    validate(req)?;
    for id in editor_default_ids()? {
        if rule_row(store, &id)?.is_none() {
            return Err(EditorGrantError::GuardError(format!(
                "the editor-defaults rule {id} is not in the store (seed failed at boot)"
            )));
        }
    }
    let ledger = rule_row(store, GRANTS_LEDGER)?
        .map(|r| r.excludes)
        .unwrap_or_default();
    let mut grants = Vec::with_capacity(req.permissions.len());
    for permission in &req.permissions {
        let tokens = subject_tokens(req, permission);
        let phases: Vec<&str> = tokens.iter().map(String::as_str).collect();
        let ctx = context(req, permission);
        let selected = wicked_governance::select_any(store, SCOPE, &phases, &ctx)
            .map_err(|e| EditorGrantError::GuardError(format!("policy select failed: {e}")))?;
        let claim = wicked_governance::decide(&selected, SCOPE, permission, &ctx, evaluated_at);
        let fired_with = |effect: wicked_governance::Effect| -> Vec<String> {
            claim
                .policy_ids
                .iter()
                .filter(|id| selected.iter().any(|p| &p.id == *id && p.effect == effect))
                .cloned()
                .collect()
        };
        let token = grant_token(&req.editor_id, &req.version, &req.sha256, permission);
        let (decision, rule_ids) = if claim.decision == wicked_apps_core::Decision::Deny {
            ("deny", fired_with(wicked_governance::Effect::Deny))
        } else if ledger.contains(&token) {
            ("allow", vec![GRANTS_LEDGER.to_string()])
        } else {
            let allows = fired_with(wicked_governance::Effect::Allow);
            if allows.is_empty() {
                ("ask", Vec::new())
            } else {
                ("allow", allows)
            }
        };
        grants.push(EditorGrant {
            permission: permission.clone(),
            decision,
            rule_ids,
            token,
        });
    }
    Ok(EditorGrants {
        editor_id: req.editor_id.clone(),
        version: req.version.clone(),
        sha256: req.sha256.clone(),
        project: req.project.clone(),
        first_party: first_party(req),
        grants,
    })
}

/// The JSON face of [`evaluate_editor_grants`] over the store at `db_path`, opened read-only — the
/// body of core-ts `Core.evaluateEditorGrants`. Errors are `bad_request: …` / `guard_error: …`.
pub fn evaluate_editor_grants_json(db_path: &str, request_json: &str) -> Result<String, String> {
    let req: EditorGrantRequest = serde_json::from_str(request_json)
        .map_err(|e| EditorGrantError::BadRequest(e.to_string()).to_string())?;
    let store = open_store_ro(Some(db_path))
        .map_err(|e| EditorGrantError::GuardError(format!("open store: {e}")).to_string())?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let grants = evaluate_editor_grants(&store, &req, now).map_err(|e| e.to_string())?;
    serde_json::to_string(&grants).map_err(|e| e.to_string())
}

/// The pack's rules, parsed from the embedded pack file.
pub(crate) fn editor_default_rules() -> anyhow::Result<Vec<wicked_governance::ConformanceRule>> {
    let doc: Value = serde_json::from_str(EDITOR_DEFAULTS_RULES)?;
    wicked_governance::normalize_bundle(&doc, "filesystem")
}

fn editor_default_ids() -> Result<Vec<String>, EditorGrantError> {
    editor_default_rules()
        .map(|rules| rules.into_iter().map(|r| r.id).collect::<Vec<_>>())
        .ok()
        .filter(|ids| !ids.is_empty())
        .ok_or_else(|| {
            EditorGrantError::GuardError("the embedded editor-defaults pack does not parse".into())
        })
}

/// Seed the pack's rules into the store, INSERT-ONLY: a rule already present (its ledger edited by
/// an approval, or retired by the operator) is left exactly as it is, so a restart never undoes an
/// approval or resurrects a retired rule. Written straight to the store (autocommit), never through
/// `register_rule`, so the actor's boot path never waits on the bus. Returns how many rules were
/// inserted.
pub(crate) fn seed_editor_defaults(
    store: &mut dyn wicked_apps_core::GraphStore,
) -> anyhow::Result<usize> {
    use wicked_apps_core::ToNode;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut nodes = Vec::new();
    for mut rule in editor_default_rules()? {
        let symbol = synthetic_symbol(wicked_governance::CONFORMANCE_RULE, &rule.id);
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
    // Autocommit, not an explicit batch (Copilot on #704): the store has no rollback, so a
    // failed batch would leave a transaction open for a later write to commit or trip over. A
    // partial seed still fails CLOSED — the grants read verifies every pack rule is present — and
    // the next boot inserts what is missing.
    store.upsert_nodes(&nodes)?;
    Ok(nodes.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wicked_apps_core::{open_store, FromNode, GraphStore, ToNode};

    const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const SHA2: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

    fn seeded() -> wicked_apps_core::SqliteStore {
        let mut store = open_store(Some(":memory:")).unwrap();
        assert_eq!(seed_editor_defaults(&mut store).unwrap(), 4);
        store
    }

    fn req(id: &str, perms: &[&str], first_party: bool) -> EditorGrantRequest {
        EditorGrantRequest {
            editor_id: id.into(),
            version: "1.0.0".into(),
            sha256: SHA.into(),
            permissions: perms.iter().map(|p| p.to_string()).collect(),
            project: None,
            first_party,
        }
    }

    fn decisions(g: &EditorGrants) -> Vec<(&str, &str)> {
        g.grants
            .iter()
            .map(|x| (x.permission.as_str(), x.decision))
            .collect()
    }

    fn approve(store: &mut dyn wicked_apps_core::GraphStore, token: &str) {
        let symbol = synthetic_symbol(wicked_governance::CONFORMANCE_RULE, GRANTS_LEDGER);
        let node = store.get_node(&symbol).unwrap().unwrap();
        let mut rule = wicked_governance::ConformanceRule::from_node(&node).unwrap();
        rule.excludes.push(token.to_string());
        store.upsert_nodes(&[rule.to_node()]).unwrap();
    }

    #[test]
    fn a_first_party_editor_gets_its_defaults_and_a_third_party_one_asks() {
        let store = seeded();
        let all = EDITOR_PERMISSIONS;
        let first = evaluate_editor_grants(&store, &req("wicked-page", &all, true), 1).unwrap();
        assert!(
            first.grants.iter().all(|g| g.decision == "allow"),
            "{:?}",
            decisions(&first)
        );
        let deck = evaluate_editor_grants(&store, &req("wicked-deck", &all, true), 1).unwrap();
        let media = deck.grants.iter().find(|g| g.permission == "network.media");
        assert_eq!(
            media.unwrap().decision,
            "ask",
            "only the page editor loads web media"
        );
        let third = evaluate_editor_grants(&store, &req("acme-notes", &all, false), 1).unwrap();
        let open = [
            "artifact.read",
            "selection.chip",
            "checks.contribute",
            "ui.fullscreen",
        ];
        for g in &third.grants {
            let want = if open.contains(&g.permission.as_str()) {
                "allow"
            } else {
                "ask"
            };
            assert_eq!(g.decision, want, "{}", g.permission);
        }
        // A forged wicked- id the caller does not vouch for is not first-party.
        let forged =
            evaluate_editor_grants(&store, &req("wicked-page", &["artifact.write"], false), 1)
                .unwrap();
        assert_eq!(decisions(&forged), vec![("artifact.write", "ask")]);
        assert!(!forged.first_party);
    }

    #[test]
    fn an_approval_is_the_exact_token_and_a_new_version_or_hash_asks_again() {
        let mut store = seeded();
        let r = req("acme-notes", &["checks.read"], false);
        let token = grant_token("acme-notes", "1.0.0", SHA, "checks.read");
        assert_eq!(
            token,
            format!("editor:acme-notes@1.0.0#{SHA}/checks.read"),
            "the full sha256 and the version"
        );
        approve(&mut store, &token);
        let got = evaluate_editor_grants(&store, &r, 1).unwrap();
        assert_eq!(decisions(&got), vec![("checks.read", "allow")]);
        assert_eq!(got.grants[0].rule_ids, vec![GRANTS_LEDGER.to_string()]);
        let mut newer = r.clone();
        newer.version = "1.0.1".into();
        assert_eq!(
            decisions(&evaluate_editor_grants(&store, &newer, 1).unwrap()),
            vec![("checks.read", "ask")]
        );
        let mut rehashed = r.clone();
        rehashed.sha256 = SHA2.into();
        assert_eq!(
            decisions(&evaluate_editor_grants(&store, &rehashed, 1).unwrap()),
            vec![("checks.read", "ask")]
        );
    }

    #[test]
    fn a_project_deny_dominates_an_approval_and_a_first_party_default() {
        let mut store = seeded();
        approve(
            &mut store,
            &grant_token("acme-notes", "1.0.0", SHA, "checks.read"),
        );
        let deny: wicked_governance::ConformanceRule = serde_json::from_value(serde_json::json!({
            "id": "OPS-KESTREL-CHECKS",
            "rule_type": "policy",
            "statement": "In Kestrel, no editor reads checks.",
            "severity": "error",
            "confidence": 1.0,
            "steering_type": "security",
            "applies_to": ["editor-perm:checks.read"],
            "effect": "deny",
            "trigger": {"contains": "\"project\":\"kestrel\""},
        }))
        .unwrap();
        let dynstore: &mut dyn GraphStore = &mut store;
        dynstore.upsert_nodes(&[deny.to_node()]).unwrap();
        for (id, first) in [("acme-notes", false), ("wicked-page", true)] {
            let mut r = req(id, &["checks.read"], first);
            r.project = Some("kestrel".into());
            let got = evaluate_editor_grants(&store, &r, 1).unwrap();
            assert_eq!(decisions(&got), vec![("checks.read", "deny")], "{id}");
            assert_eq!(
                got.grants[0].rule_ids,
                vec!["OPS-KESTREL-CHECKS".to_string()]
            );
            r.project = Some("other".into());
            let got = evaluate_editor_grants(&store, &r, 1).unwrap();
            assert_eq!(
                decisions(&got),
                vec![("checks.read", "allow")],
                "{id} elsewhere"
            );
        }
    }

    #[test]
    fn the_seed_is_insert_only() {
        let mut store = seeded();
        let token = grant_token("acme-notes", "1.0.0", SHA, "artifact.write");
        approve(&mut store, &token);
        assert_eq!(
            seed_editor_defaults(&mut store).unwrap(),
            0,
            "nothing re-inserted"
        );
        let symbol = synthetic_symbol(wicked_governance::CONFORMANCE_RULE, GRANTS_LEDGER);
        let rule = wicked_governance::ConformanceRule::from_node(
            &store.get_node(&symbol).unwrap().unwrap(),
        )
        .unwrap();
        assert!(
            rule.excludes.contains(&token),
            "a restart never undoes an approval"
        );
    }

    #[test]
    fn bad_requests_and_an_unseeded_store_fail_closed() {
        let store = seeded();
        let mut short = req("acme-notes", &["checks.read"], false);
        short.sha256 = SHA[..12].into();
        assert!(matches!(
            evaluate_editor_grants(&store, &short, 1),
            Err(EditorGrantError::BadRequest(m)) if m.contains("64-hex")
        ));
        let unknown = req("acme-notes", &["network.fetch"], false);
        assert!(matches!(
            evaluate_editor_grants(&store, &unknown, 1),
            Err(EditorGrantError::BadRequest(_))
        ));
        let bad_id = req("../x", &["checks.read"], false);
        assert!(evaluate_editor_grants(&store, &bad_id, 1).is_err());
        let empty = open_store(Some(":memory:")).unwrap();
        assert!(matches!(
            evaluate_editor_grants(&empty, &req("acme-notes", &["checks.read"], false), 1),
            Err(EditorGrantError::GuardError(m)) if m.contains("not in the store")
        ));
    }

    #[test]
    fn the_json_face_reads_camel_case_and_refuses_unknown_fields() {
        let dir = std::env::temp_dir().join(format!("ep-k1-json-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("g.db").to_string_lossy().into_owned();
        {
            let mut store = open_store(Some(db.as_str())).unwrap();
            seed_editor_defaults(&mut store).unwrap();
        }
        let body = serde_json::json!({
            "editorId": "wicked-page", "version": "2.0.0", "sha256": SHA,
            "permissions": ["artifact.write"], "firstParty": true
        })
        .to_string();
        let v: Value =
            serde_json::from_str(&evaluate_editor_grants_json(&db, &body).unwrap()).unwrap();
        assert_eq!(v["grants"][0]["decision"], "allow");
        assert_eq!(
            v["grants"][0]["ruleIds"],
            serde_json::json!(["EDITOR-BUILTIN"])
        );
        assert_eq!(
            v["grants"][0]["token"],
            format!("editor:wicked-page@2.0.0#{SHA}/artifact.write")
        );
        let typo = body.replace("firstParty", "first_party");
        assert!(evaluate_editor_grants_json(&db, &typo)
            .unwrap_err()
            .starts_with("bad_request"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
