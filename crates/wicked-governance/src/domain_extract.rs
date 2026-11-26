//! J6 v3: the domain model garden's `domain-run` writes, read into domain-model 1.2.0 (DES-C2-v2).
//!
//! garden `scripts/domain/store.py` (`rows_for`, :303-381) is the only writer of four annotation
//! types; core adds the fifth, `rule_ruling`, through [`write_ruling`]. This module is their one
//! reader: the v3 builder ([`build_extract_model`]) and the coverage report (through
//! [`ExtractIndex::rule_counts`]) classify a statement the same way, so the model view and the
//! report never tell two stories.
//!
//! A statement's effective status is one of `Checked`, `open`, `confirmed`, `rejected` or `stale`.
//! `stale` overrides the rest: the code its target was checked against changed (garden's own carry
//! rule, `store.py:176-186`: the exact bytes of every batch file), so nothing it says is shown as
//! checked until the next run checks it again.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use wicked_apps_core::GraphRead;

/// The schema version a v3 build writes.
pub const SCHEMA_V3: &str = "1.2.0";
pub const RULE_ANN: &str = "business_rule";
pub const TARGET_ANN: &str = "domain_target";
pub const GROUP_ANN: &str = "rule_group";
pub const FOLLOWUP_ANN: &str = "domain_followup";
pub const RULING_ANN: &str = "rule_ruling";
/// What a ruling's author column says.
pub const RULING_AUTHOR: &str = "wicked-core/domain-ruling";
const RETIRED: &str = "retired";

/// A statement's identity for a ruling (R04): `v1:` + sha256 of `[anchor file, claims sorted by
/// index as [i, type, text, when, unless, of]]`. Unlike garden's `content_hash` it includes each
/// claim's index, which `when`, `unless` and `of` point at, so a ruling never survives a change of
/// which claim a guard names. Line numbers are not part of it: a statement that only moved keeps
/// its key (its `target_hash` still binds the ruling to the code).
pub fn statement_key(contract: &Value) -> String {
    let file = contract
        .get("anchor")
        .and_then(|a| a.get("file"))
        .cloned()
        .unwrap_or(Value::Null);
    let mut claims: Vec<Value> = contract
        .get("claims")
        .and_then(Value::as_array)
        .map(|cs| {
            cs.iter()
                .filter(|c| c.is_object())
                .map(|c| {
                    json!([
                        c.get("i").cloned().unwrap_or(Value::Null),
                        c.get("type").cloned().unwrap_or(Value::Null),
                        c.get("text").cloned().unwrap_or(Value::Null),
                        c.get("when").cloned().unwrap_or(Value::Null),
                        c.get("unless").cloned().unwrap_or(Value::Null),
                        c.get("of").cloned().unwrap_or(Value::Null),
                    ])
                })
                .collect()
        })
        .unwrap_or_default();
    // by index (a number, R04), then by the whole tuple so two claims with one index still order
    claims.sort_by_cached_key(|c| (c[0].as_u64().unwrap_or(u64::MAX), c.to_string()));
    let canon = json!([file, claims]).to_string();
    format!("v1:{}", hex(&Sha256::digest(canon.as_bytes())))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `root/f` when it stays inside `root` (garden `graph.inside`), else `None`.
fn inside(root: &Path, f: &str) -> Option<std::path::PathBuf> {
    if f.is_empty() || Path::new(f).is_absolute() {
        return None;
    }
    let base = std::fs::canonicalize(root).ok()?;
    let p = std::fs::canonicalize(base.join(f)).ok()?;
    p.starts_with(&base).then_some(p).filter(|p| p != &base)
}

/// garden `store.batch_hash` (store.py:69-79), byte for byte: for each file in sorted order, its
/// name, a NUL, its exact bytes (or NUL "missing"), a NUL.
pub fn batch_hash(root: &Path, files: &[String]) -> String {
    let set: BTreeSet<&str> = files.iter().map(String::as_str).collect();
    let mut h = Sha256::new();
    for f in set {
        h.update(f.as_bytes());
        h.update(b"\0");
        match inside(root, f).and_then(|p| std::fs::read(p).ok()) {
            Some(bytes) => h.update(&bytes),
            None => h.update(b"\0missing"),
        }
        h.update(b"\0");
    }
    hex(&h.finalize())
}

/// One live row: the node it is on, its value, and the order it was written in.
#[derive(Debug, Clone)]
pub struct Row {
    pub symbol: String,
    pub value: Value,
    ts: i64,
    seq: usize,
}

/// Rows skipped by the live-record policy, by why (reported on the build, never fatal).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skipped {
    pub retired: u64,
    pub malformed: u64,
    pub old_loop: u64,
}

/// The live rows of one annotation type by key (the part after `<type>:`). The live-record policy
/// (DES-C2-v2 §1.1, R01): a row's value must be a JSON object; one row per key, the latest written
/// (`ts`, then write order) winning, and a `{"status": "retired"}` row retiring its key when it is
/// the latest. A row on a node the index no longer holds (its file was removed or re-indexed
/// without it) is an orphan and is not read, as garden's reader joins to `nodes` (store.py:234).
///
/// `v3_seen` is set by any row only J6 v3 writes, whatever becomes of it here (live, retired,
/// malformed or an orphan): a row of a v3-only type, or a `business_rule` row in the v3 shape.
fn live_rows(
    store: &dyn GraphRead,
    ty: &str,
    skipped: &mut Skipped,
    present: &mut BTreeMap<String, bool>,
    v3_seen: &mut bool,
) -> anyhow::Result<BTreeMap<String, Row>> {
    let mut latest: BTreeMap<String, Row> = BTreeMap::new();
    let prefix = format!("{ty}:");
    for (seq, (sym, a)) in store.annotations_by_type(ty)?.into_iter().enumerate() {
        // Parsed only until the first one: a large 1.1.0 store's rules are plain text.
        *v3_seen = *v3_seen || ty != RULE_ANN || is_v3_value(&a.value);
        let here = match present.get(sym.as_str()) {
            Some(h) => *h,
            None => {
                let h = store.get_node(&sym)?.is_some();
                present.insert(sym.to_string(), h);
                h
            }
        };
        if !here {
            continue;
        }
        let Some(name) = a.key.strip_prefix(&prefix) else {
            // A rule keyed the old loop's way is a rule from before J6 v3, not a broken v3 row.
            if ty == RULE_ANN {
                skipped.old_loop += 1;
            } else {
                skipped.malformed += 1;
            }
            continue;
        };
        let value = match serde_json::from_str::<Value>(&a.value) {
            Ok(v @ Value::Object(_)) => v,
            _ => {
                skipped.malformed += 1;
                continue;
            }
        };
        let row = Row {
            symbol: sym.to_string(),
            value,
            ts: a.ts,
            seq,
        };
        match latest.get(name) {
            Some(prev) if (prev.ts, prev.seq) > (row.ts, row.seq) => {}
            _ => {
                latest.insert(name.to_string(), row);
            }
        }
    }
    latest.retain(|_, r| {
        let retired = r.value.get("status").and_then(Value::as_str) == Some(RETIRED);
        if retired {
            skipped.retired += 1;
        }
        !retired
    });
    Ok(latest)
}

/// A statement's effective status (DES-C2-v2 §3.2, R12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Effective {
    Checked,
    Open,
    Confirmed,
    Rejected,
    Stale,
}

impl Effective {
    pub fn label(self) -> &'static str {
        match self {
            Effective::Checked => "Checked",
            Effective::Open => "open",
            Effective::Confirmed => "confirmed",
            Effective::Rejected => "rejected",
            Effective::Stale => "stale",
        }
    }
    /// Whether the statement settles its part of its target.
    fn settled(self) -> bool {
        matches!(self, Effective::Checked | Effective::Confirmed)
    }
}

/// A statement as the index read it.
#[derive(Debug, Clone)]
pub struct Statement {
    pub id: String,
    pub row: Row,
    pub contract: Value,
    pub key: String,
    pub target: Option<String>,
    pub raw: String,
    pub effective: Effective,
    /// The live ruling that applies to it, if any.
    pub ruling: Option<Value>,
}

/// A target as the index read it.
#[derive(Debug, Clone)]
pub struct Target {
    pub id: String,
    pub row: Row,
    pub fresh: bool,
    pub ruling: Option<Value>,
    pub accounted: bool,
}

/// Everything a v3 reader needs, read once (the builder and the coverage report share it).
#[derive(Debug, Clone, Default)]
pub struct ExtractIndex {
    pub statements: BTreeMap<String, Statement>,
    pub targets: BTreeMap<String, Target>,
    pub groups: BTreeMap<String, Row>,
    pub followups: BTreeMap<String, Row>,
    pub rulings: BTreeMap<String, Row>,
    pub skipped: Skipped,
    /// (core#1955 R2-2) Whether this is a v3 store: a 1.2.0 build is persisted, or the store holds
    /// any row only J6 v3 writes, live or not (a store whose v3 rows are all retired is still one).
    /// `false` only for a store the old loop alone wrote, whose stored 1.1.0 graph keeps its label.
    pub holds_v3: bool,
}

fn str_of<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

/// Whether a ruling applies to a statement (§3.1, R04/R05).
fn statement_ruling_applies(r: &Value, key: &str, target_hash: Option<&str>) -> bool {
    matches!(str_of(r, "action"), Some("confirm" | "reject"))
        && str_of(r, "statement_key") == Some(key)
        && target_hash.is_some()
        && str_of(r, "target_hash") == target_hash
}

impl ExtractIndex {
    /// Read the store. `repo_root` is the checkout the build judges freshness against; with `None`
    /// no target is fresh, so no v3 statement counts as checked (a report with no checkout).
    pub fn load(store: &dyn GraphRead, repo_root: Option<&Path>) -> anyhow::Result<Self> {
        let mut skipped = Skipped::default();
        let mut present = BTreeMap::new();
        // (core#1955 R2-2) A 1.2.0 build persisted, or any row only v3 writes: a v3 store.
        let mut v3 =
            crate::domain_model::persisted_schema_version(store)?.as_deref() == Some(SCHEMA_V3);
        let seen = &mut v3;
        let mut rules = live_rows(store, RULE_ANN, &mut skipped, &mut present, seen)?;
        let target_rows = live_rows(store, TARGET_ANN, &mut skipped, &mut present, seen)?;
        let mut groups = live_rows(store, GROUP_ANN, &mut skipped, &mut present, seen)?;
        let mut followups = live_rows(store, FOLLOWUP_ANN, &mut skipped, &mut present, seen)?;
        let rulings = live_rows(store, RULING_ANN, &mut skipped, &mut present, seen)?;
        // A row missing what every reader needs is malformed: a group with no `group` object, a
        // follow-up with no `kind`.
        groups.retain(|_, r| {
            let ok = r.value.get("group").is_some_and(Value::is_object);
            skipped.malformed += u64::from(!ok);
            ok
        });
        followups.retain(|_, r| {
            let ok = str_of(&r.value, "kind").is_some_and(|k| !k.trim().is_empty());
            skipped.malformed += u64::from(!ok);
            ok
        });
        rules.retain(|_, r| {
            let v3 = r.value.get("contract").is_some_and(Value::is_object);
            if !v3 {
                skipped.old_loop += 1;
            }
            v3
        });

        let mut targets: BTreeMap<String, Target> = BTreeMap::new();
        for (id, row) in target_rows {
            let files: Vec<String> = row
                .value
                .get("batch_files")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|f| f.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let stored = str_of(&row.value, "batch_hash");
            // No checkout to judge against: nothing can be shown as checked (fail closed).
            let fresh = match (repo_root, stored) {
                (Some(root), Some(h)) => batch_hash(root, &files) == h,
                _ => false,
            };
            let ruling = rulings
                .get(&format!("target:{id}"))
                .map(|r| r.value.clone())
                .filter(|r| {
                    matches!(str_of(r, "action"), Some("confirm" | "reject"))
                        && str_of(r, "target_hash").is_some()
                        && str_of(r, "target_hash") == str_of(&row.value, "target_hash")
                });
            targets.insert(
                id.clone(),
                Target {
                    id,
                    row,
                    fresh,
                    ruling,
                    accounted: false,
                },
            );
        }

        let mut statements = BTreeMap::new();
        for (id, row) in rules {
            let contract = row.value["contract"].clone();
            // Coherence (R09): the contract names its own id and target when it has them.
            if str_of(&contract, "id").is_some_and(|c| c != id) {
                skipped.malformed += 1;
                continue;
            }
            let raw = match str_of(&row.value, "status") {
                Some(s @ ("Checked" | "open"))
                    if str_of(&row.value, "statement").is_some_and(|t| !t.trim().is_empty()) =>
                {
                    s.to_string()
                }
                _ => {
                    skipped.malformed += 1;
                    continue;
                }
            };
            let key = statement_key(&contract);
            let target = str_of(&contract, "target").map(String::from);
            // Fresh only when its target is, and the two rows are of one write: a read that saw a
            // statement from one run and its target from the next fails closed (stale).
            let fresh = target
                .as_deref()
                .and_then(|t| targets.get(t))
                .is_some_and(|t| {
                    t.fresh
                        && ["batch_hash", "target_hash"].iter().all(|k| {
                            str_of(&row.value, k).is_some()
                                && str_of(&row.value, k) == str_of(&t.row.value, k)
                        })
                });
            let mut ruling = None;
            let effective = if !fresh {
                Effective::Stale
            } else if raw == "Checked" {
                Effective::Checked
            } else {
                let r = rulings
                    .get(&format!("statement:{id}"))
                    .map(|r| r.value.clone())
                    .filter(|r| {
                        statement_ruling_applies(r, &key, str_of(&row.value, "target_hash"))
                    });
                let eff = match r.as_ref().and_then(|r| str_of(r, "action")) {
                    Some("confirm") => Effective::Confirmed,
                    Some("reject") => Effective::Rejected,
                    _ => Effective::Open,
                };
                ruling = r;
                eff
            };
            statements.insert(
                id.clone(),
                Statement {
                    id,
                    row,
                    contract,
                    key,
                    target,
                    raw,
                    effective,
                    ruling,
                },
            );
        }

        // A target is accounted when it is fresh, its lines are accounted for (or a confirm says
        // they make no business decision), and every statement it lists is present and settled.
        for t in targets.values_mut() {
            let unaccounted = t
                .row
                .value
                .get("state")
                .and_then(|s| s.get("unaccounted"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let lines_ok = !unaccounted
                || t.ruling
                    .as_ref()
                    .is_some_and(|r| str_of(r, "action") == Some("confirm"));
            let listed: Vec<&str> = t
                .row
                .value
                .get("statements")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let all_settled = listed.iter().all(|s| {
                statements
                    .get(*s)
                    .is_some_and(|st: &Statement| st.effective.settled())
            });
            t.accounted = t.fresh && lines_ok && all_settled;
        }

        Ok(ExtractIndex {
            statements,
            targets,
            groups,
            followups,
            rulings,
            skipped,
            holds_v3: v3,
        })
    }

    /// Whether the index holds no live statement, target, group or follow-up (rulings are not
    /// checked; retired, malformed and orphan rows are never read). Not "a v3 store": that is
    /// [`ExtractIndex::holds_v3`].
    pub fn is_empty(&self) -> bool {
        self.statements.is_empty()
            && self.targets.is_empty()
            && self.groups.is_empty()
            && self.followups.is_empty()
    }

    /// For the coverage report (R22): how the `business_rule` annotation (`key`, `value`) on a node
    /// counts, by the same effective status the model shows.
    ///
    /// An old-loop row (no `contract`) counts the 1.1.0 way only in a store that is not a v3 store
    /// ([`ExtractIndex::holds_v3`]): there it is the stored 1.1.0 graph's own rule (core#1924 keeps
    /// those readable). In a v3 store the 1.2.0 model skips it (`skipped.old_loop`), so the report
    /// does too and the two never tell different stories (core#1955 R2-2).
    pub fn rule_counts(&self, key: &str, value: &str) -> RuleCount {
        let v3_shaped = is_v3_value(value);
        let not_v3 = if self.holds_v3 {
            RuleCount::Ignored
        } else {
            RuleCount::Legacy
        };
        let Some(id) = key.strip_prefix("business_rule:") else {
            return if v3_shaped {
                RuleCount::Ignored
            } else {
                not_v3
            };
        };
        match self.statements.get(id) {
            // The live row for this key: its status decides (a stale, open or rejected statement
            // is accounted but never resolved).
            Some(s) if serde_json::from_str::<Value>(value).ok().as_ref() == Some(&s.row.value) => {
                if s.effective.settled() {
                    RuleCount::Resolved(confidence_of(s.effective))
                } else {
                    RuleCount::Unresolved
                }
            }
            // An older copy of a key that is live elsewhere, a tombstone, or a skipped v3 row.
            _ if v3_shaped => RuleCount::Ignored,
            _ => not_v3,
        }
    }
}

/// How one `business_rule` annotation counts in the coverage report.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RuleCount {
    /// A v3 statement that is effectively Checked or confirmed, with its confidence.
    Resolved(f64),
    /// A v3 statement that is open, rejected or stale: accounted, not resolved.
    Unresolved,
    /// A v3 row that is not the live one (retired, superseded or malformed), or an old-loop row in
    /// a v3 store: not counted at all.
    Ignored,
    /// Not a v3 row, in a store that is not a v3 store: the 1.1.0 reading (its confidence) applies.
    Legacy,
}

/// Whether an annotation value is a v3 row or a v3 tombstone (the 1.1.0 builder skips these).
pub fn is_v3_value(value: &str) -> bool {
    serde_json::from_str::<Value>(value).is_ok_and(|v| {
        v.get("contract").is_some() || v.get("status").and_then(Value::as_str) == Some(RETIRED)
    })
}

/// `file#La-Lb` of a statement's anchor, when it is usable.
fn anchor_of(contract: &Value) -> Option<(String, u64, u64)> {
    let a = contract.get("anchor")?;
    let file = a.get("file")?.as_str()?.to_string();
    if !repo_relative(&file) {
        return None;
    }
    let l = a.get("lines")?.as_array()?;
    if l.len() != 2 {
        return None;
    }
    let (x, y) = (l[0].as_u64()?, l[1].as_u64()?);
    (x >= 1 && y >= 1).then(|| (file, x.min(y), x.max(y)))
}

/// A usable repo-relative path: not empty, not absolute, no `..` step.
fn repo_relative(f: &str) -> bool {
    !f.trim().is_empty()
        && !Path::new(f).is_absolute()
        && !f.starts_with('/')
        && !f.split(['/', '\\']).any(|p| p == "..")
}

/// `"f:a-b"` / `"f:a"` (garden `lookup.pref`) as `{file, from, to}`.
fn range_of(r: &str) -> Option<Value> {
    let r = r.trim();
    let (file, rest) = r.rsplit_once(':')?;
    if !repo_relative(file) || file.chars().any(char::is_whitespace) {
        return None;
    }
    let (a, b) = match rest.split_once('-') {
        Some((a, b)) => (a.trim().parse::<u64>().ok()?, b.trim().parse::<u64>().ok()?),
        None => {
            let a = rest.trim().parse::<u64>().ok()?;
            (a, a)
        }
    };
    (a >= 1 && b >= 1).then(|| json!({"file": file, "from": a.min(b), "to": a.max(b)}))
}

/// A claim as the doc carries it: the writer's fields, with `lines` and `guards` normalized to
/// `ranges` / `guard_ranges` (R22: inclusive, 1-based).
fn claim_view(c: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for k in ["i", "type", "text", "when", "unless", "of"] {
        if let Some(v) = c.get(k).filter(|v| !v.is_null()) {
            out.insert(k.into(), v.clone());
        }
    }
    for (src, dst) in [("lines", "ranges"), ("guards", "guard_ranges")] {
        let rs: Vec<Value> = c
            .get(src)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|r| r.as_str().and_then(range_of))
                    .collect()
            })
            .unwrap_or_default();
        if !rs.is_empty() || src == "lines" {
            out.insert(dst.into(), Value::Array(rs));
        }
    }
    Value::Object(out)
}

/// The v3 fields a 1.2.0 business rule carries beside the 1.1.0 ones.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatementView {
    pub statement_key: String,
    pub status: String,
    pub located: bool,
    pub target: Option<String>,
    pub claims: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ruling: Option<Value>,
}

/// The v3 part of a 1.2.0 model's metadata.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExtractMeta {
    pub accounting: Accounting,
    pub rule_groups: Vec<Value>,
    pub followups: Vec<Value>,
    pub targets: Vec<Value>,
    /// Every emitted statement no shown sentence of a business group cites (garden#1545): one a
    /// housekeeping group or only a withheld sentence cites, or none (an open statement, whose
    /// follow-up names it). A requirement is a business decision; these stay readable by id (a
    /// group cites them, a follow-up's ruling names their key) without being one. Absent on a
    /// model persisted before it.
    #[serde(default)]
    pub statements: Vec<crate::domain_model::Rule>,
    /// When the build judged freshness, unix seconds (R06: the model is as of this time).
    pub built_at: i64,
    pub skipped: Skipped,
}

/// `metadata.accounting` (§3.2): targets, the accounted ones, open statements (open or rejected),
/// stale targets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Accounting {
    pub targets: u64,
    pub accounted: u64,
    pub open: u64,
    pub stale: u64,
}

fn confidence_of(e: Effective) -> f64 {
    match e {
        Effective::Confirmed => 1.0,
        Effective::Checked => 0.9,
        _ => 0.4,
    }
}

fn ruling_view(r: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for k in ["action", "note", "by", "at"] {
        if let Some(v) = r.get(k) {
            out.insert(k.into(), v.clone());
        }
    }
    Value::Object(out)
}

/// The file stem a domain falls back to as its label.
fn stem(file: &str) -> String {
    let base = file.rsplit('/').next().unwrap_or(file);
    match base.rsplit_once('.') {
        Some((s, _)) if !s.is_empty() => s.to_string(),
        _ => base.to_string(),
    }
}

/// A statement as the model shows it: in a requirement's `business_rules`, or in `statements`.
fn rule_of(st: &Statement) -> crate::domain_model::Rule {
    use crate::domain_model::{Provenance, Rule};
    let anchor = anchor_of(&st.contract);
    let source_ref = anchor.as_ref().map(|(f, a, b)| format!("{f}#L{a}-L{b}"));
    let claims = st
        .contract
        .get("claims")
        .and_then(Value::as_array)
        .map(|cs| {
            cs.iter()
                .filter(|c| c.is_object())
                .map(claim_view)
                .collect()
        })
        .unwrap_or_default();
    Rule {
        id: st.id.clone(),
        statement: str_of(&st.row.value, "statement")
            .unwrap_or_default()
            .to_string(),
        confidence: confidence_of(st.effective),
        provenance: Provenance {
            source: "domain-run".into(),
            reference: source_ref
                .clone()
                .or_else(|| st.target.clone())
                .unwrap_or_else(|| st.id.clone()),
            source_kinds: vec!["code-body".into()],
        },
        source_ref,
        extract: Some(StatementView {
            statement_key: st.key.clone(),
            status: st.effective.label().into(),
            located: anchor.is_some(),
            target: st.target.clone(),
            claims,
            checked_by: match st.effective {
                Effective::Checked => Some("codex".into()),
                Effective::Confirmed => Some("you".into()),
                _ => None,
            },
            ruling: st.ruling.as_ref().map(ruling_view),
        }),
    }
}

/// Build a 1.2.0 model from the store's v3 rows (DES-C2-v2 §2). Requirement keys are provisional
/// (`REQ-NNN` in anchor order within each file domain); the caller stabilizes them against the
/// previous build without carrying absent ones.
pub fn build_extract_model(
    store: &dyn GraphRead,
    repo_root: &Path,
    built_at: i64,
) -> anyhow::Result<crate::domain_model::DomainModel> {
    use crate::domain_model::{Domain, DomainModel, Metadata, Requirement, Rule};
    let idx = ExtractIndex::load(store, Some(repo_root))?;

    // Groups: validated against their cited statements (R11), business first, housekeeping last.
    // Each group with the statements its sentences cite that this build withheld because the code
    // or a statement moved since the writer's check (shown sentences, as written).
    let mut groups: Vec<(String, Value, Vec<String>)> = Vec::new();
    for (id, row) in &idx.groups {
        let Some(g) = row.value.get("group").filter(|g| g.is_object()) else {
            continue;
        };
        let mut g = g.clone();
        let inputs = row.value.get("inputs").and_then(Value::as_object);
        let mut withheld: Vec<Value> = g
            .get("withheld")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let sound = |s: &Value| -> bool {
            let cites: Vec<&str> = s
                .get("cites")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            !cites.is_empty()
                && cites.iter().all(|c| {
                    idx.statements.get(*c).is_some_and(|st| {
                        st.effective == Effective::Checked
                            && inputs.is_some_and(|inp| {
                                // the writer keeps each input as the sha256 of the statement's text
                                // (garden run.py:173 `store.sha(r["text"])`); a missing input
                                // cannot show the sentence rests on the statement as it reads now
                                let read = inp.get(*c).and_then(Value::as_str);
                                read.is_some()
                                    && read
                                        == str_of(&st.row.value, "statement")
                                            .map(|t| hex(&Sha256::digest(t.as_bytes())))
                                            .as_deref()
                            })
                    })
                })
        };
        let changed = |s: &Value| -> Value {
            json!({
                "id": s.get("id").cloned().unwrap_or(Value::Null),
                "layer": if s.get("id").and_then(Value::as_str).is_some_and(|i| i.ends_with(".r")) { "rule" } else { "detail" },
                "text": s.get("text").cloned().unwrap_or(Value::Null),
                "reason": "its statements changed since it was written",
                "cites": s.get("cites").cloned().unwrap_or(json!([])),
            })
        };
        let mut moved: Vec<Value> = Vec::new();
        if let Some(rule) = g.get("rule").filter(|r| r.is_object()).cloned() {
            if !sound(&rule) {
                moved.push(rule);
                g["rule"] = Value::Null;
            }
        }
        if let Some(details) = g.get("details").and_then(Value::as_array).cloned() {
            let (keep, drop): (Vec<Value>, Vec<Value>) = details.into_iter().partition(sound);
            moved.extend(drop);
            g["details"] = Value::Array(keep);
        }
        withheld.extend(moved.iter().map(changed));
        let moved: Vec<String> = moved
            .iter()
            .flat_map(|s| {
                s.get("cites")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
            })
            .filter_map(Value::as_str)
            .map(String::from)
            .collect();
        // Stale when any statement it cites, shown or withheld, rests on code that changed.
        let stale = withheld
            .iter()
            .chain(g.get("rule").filter(|r| r.is_object()))
            .chain(
                g.get("details")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten(),
            )
            .flat_map(|s| {
                s.get("cites")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
            })
            .filter_map(Value::as_str)
            .any(|c| {
                idx.statements
                    .get(c)
                    .is_some_and(|st| st.effective == Effective::Stale)
            });
        g["withheld"] = Value::Array(withheld);
        if let Some(o) = g.as_object_mut() {
            o.insert("id".into(), json!(id));
            o.insert("stale".into(), json!(stale));
            if let Some(d) = row.value.get("desc") {
                o.insert("desc".into(), d.clone());
            }
        }
        groups.push((id.clone(), g, moved));
    }
    groups.sort_by_key(|(id, g, _)| {
        (
            str_of(g, "kind") == Some("housekeeping"),
            g.get("g").and_then(Value::as_i64).unwrap_or(i64::MAX),
            id.clone(),
        )
    });
    // The business groups whose sentences cite each statement, by display index: only these make a
    // statement a requirement (garden#1545). A housekeeping group is plumbing, and a sentence the
    // writer's check withheld was never shown. A shown sentence this build withheld because its
    // statements moved still counts, so an edit shows its requirement stale instead of hiding it.
    let mut citing: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (gi, (_, g, moved)) in groups.iter().enumerate() {
        if str_of(g, "kind") != Some("business") {
            continue;
        }
        let mut cites: BTreeSet<&str> = moved.iter().map(String::as_str).collect();
        for s in std::iter::once(g.get("rule")).flatten().chain(
            g.get("details")
                .and_then(Value::as_array)
                .into_iter()
                .flatten(),
        ) {
            for c in s
                .get("cites")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(c) = c.as_str() {
                    cites.insert(c);
                }
            }
        }
        for c in cites {
            citing.entry(c).or_default().push(gi);
        }
    }

    // Requirements: one per node holding an emitted (non-rejected) statement a shown business
    // sentence cites, in a file domain. Every other emitted statement goes to `statements`.
    struct Draft<'a> {
        file: String,
        first: u64,
        statements: Vec<&'a Statement>,
    }
    let mut drafts: BTreeMap<&str, Draft> = BTreeMap::new();
    let mut loose: Vec<(String, u64, &Statement)> = Vec::new();
    for st in idx.statements.values() {
        if st.effective == Effective::Rejected {
            continue;
        }
        let anchor = anchor_of(&st.contract);
        let file = anchor
            .as_ref()
            .map(|a| a.0.clone())
            .or_else(|| {
                st.target
                    .as_deref()
                    .and_then(|t| idx.targets.get(t))
                    .and_then(|t| str_of(&t.row.value, "file").map(String::from))
            })
            .unwrap_or_default();
        let first = anchor.as_ref().map(|a| a.1).unwrap_or(u64::MAX);
        if !citing.contains_key(st.id.as_str()) {
            loose.push((file, first, st));
            continue;
        }
        let d = drafts.entry(st.row.symbol.as_str()).or_insert(Draft {
            file,
            first,
            statements: Vec::new(),
        });
        d.first = d.first.min(first);
        d.statements.push(st);
    }
    let mut domains: BTreeMap<String, Domain> = BTreeMap::new();
    // Per file domain: each citing group counted once (by display index) toward its area.
    let mut voted: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
    let mut ordered: Vec<(&str, Draft)> = drafts.into_iter().collect();
    ordered.sort_by(|a, b| (&a.1.file, a.1.first, a.0).cmp(&(&b.1.file, b.1.first, b.0)));
    for (symbol, mut d) in ordered {
        d.statements.sort_by_cached_key(|st| natural(&st.id));
        let node = store.get_node(&wicked_apps_core::SymbolId::from(symbol))?;
        // The groups citing any of its statements, in display order: the first titles it.
        let cited_by: BTreeSet<usize> = d
            .statements
            .iter()
            .flat_map(|st| citing.get(st.id.as_str()).into_iter().flatten().copied())
            .collect();
        let title = cited_by
            .iter()
            .find_map(|gi| str_of(&groups[*gi].1, "title").filter(|t| !t.trim().is_empty()))
            .map(String::from);
        voted.entry(d.file.clone()).or_default().extend(cited_by);
        let business_rules: Vec<Rule> = d.statements.iter().map(|st| rule_of(st)).collect();
        let name = node.as_ref().map(|n| n.name.clone());
        let domain = domains.entry(d.file.clone()).or_default();
        let key = format!("REQ-{:03}", domain.requirements.len() + 1);
        domain.requirements.insert(
            key,
            Requirement {
                title: title
                    .or(name)
                    .filter(|t| !t.trim().is_empty())
                    .unwrap_or_else(|| d.file.clone()),
                description: business_rules
                    .first()
                    .map(|r| r.statement.clone())
                    .unwrap_or_default(),
                business_rules,
                code_symbol: Some(symbol.to_string()),
                ..Default::default()
            },
        );
    }
    // A domain's label: the area most of its citing groups name, ties to the earliest group.
    for (file, domain) in domains.iter_mut() {
        let mut areas: BTreeMap<&str, (u64, usize)> = BTreeMap::new();
        for gi in voted.get(file).into_iter().flatten() {
            if let Some(area) = str_of(&groups[*gi].1, "area").filter(|a| !a.trim().is_empty()) {
                let e = areas.entry(area).or_insert((0, *gi));
                e.0 += 1;
                e.1 = e.1.min(*gi);
            }
        }
        let label = areas
            .into_iter()
            .max_by(|a, b| a.1 .0.cmp(&b.1 .0).then(b.1 .1.cmp(&a.1 .1)))
            .map(|(area, _)| area.to_string());
        domain.description = Some(label.unwrap_or_else(|| stem(file)));
    }

    // The statements no requirement holds, in anchor order (file, first line, node, then id).
    loose.sort_by_cached_key(|(file, first, st)| {
        (file.clone(), *first, st.row.symbol.clone(), natural(&st.id))
    });
    let statements: Vec<Rule> = loose.into_iter().map(|(_, _, st)| rule_of(st)).collect();

    // Follow-ups, in their writer's order, each with its subject's live ruling.
    let mut followups: Vec<(i64, String, Value)> = idx
        .followups
        .iter()
        .map(|(id, row)| {
            let mut v = row.value.clone();
            let ruling = match (str_of(&v, "statement"), str_of(&v, "kind")) {
                (Some(s), _) => idx.statements.get(s).and_then(|s| s.ruling.clone()),
                (None, Some("refused")) => str_of(&v, "target")
                    .and_then(|t| idx.targets.get(t))
                    .filter(|t| t.fresh)
                    .and_then(|t| t.ruling.clone()),
                _ => None,
            };
            if let Some(o) = v.as_object_mut() {
                o.insert("id".into(), json!(id));
                if let Some(r) = ruling {
                    o.insert("ruling".into(), ruling_view(&r));
                }
            }
            (
                v.get("n").and_then(Value::as_i64).unwrap_or(i64::MAX),
                id.clone(),
                v,
            )
        })
        .collect();
    followups.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));

    // Targets and the accounting.
    let mut accounting = Accounting::default();
    let mut targets = Vec::new();
    for t in idx.targets.values() {
        accounting.targets += 1;
        accounting.accounted += u64::from(t.accounted);
        accounting.stale += u64::from(!t.fresh);
        let listed: Vec<Value> = t
            .row
            .value
            .get("statements")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let rejected: Vec<Value> = listed
            .iter()
            .filter_map(Value::as_str)
            .filter_map(|s| idx.statements.get(s))
            .filter(|s| s.effective == Effective::Rejected)
            .map(|s| {
                json!({
                    "id": s.id,
                    "statement": str_of(&s.row.value, "statement"),
                    // what a later ruling on it names as `expect_key` (a reject can be undone)
                    "statement_key": s.key,
                    "ruling": s.ruling.as_ref().map(ruling_view),
                })
            })
            .collect();
        let v = &t.row.value;
        let mut tv = json!({
            "id": t.id,
            "file": v.get("file"),
            "lines": v.get("lines"),
            "target_hash": v.get("target_hash"),
            "accounted": t.accounted,
            "stale": !t.fresh,
            "unaccounted_reasons": v.get("state").and_then(|s| s.get("tfails")).cloned().unwrap_or(json!([])),
            "statements": listed,
            "rejected": rejected,
        });
        if let Some(r) = t.ruling.as_ref().filter(|_| t.fresh) {
            tv["ruling"] = ruling_view(r);
        }
        targets.push(tv);
    }
    accounting.open = idx
        .statements
        .values()
        .filter(|s| matches!(s.effective, Effective::Open | Effective::Rejected))
        .count() as u64;

    Ok(DomainModel {
        metadata: Metadata {
            schema_version: SCHEMA_V3.into(),
            migration_mode: "functional".into(),
            source: Some("estate-graph".into()),
            coverage: None,
            product_only: None,
            business_domains: Vec::new(),
            extract: Some(ExtractMeta {
                accounting,
                rule_groups: groups.into_iter().map(|(_, g, _)| g).collect(),
                followups: followups.into_iter().map(|(_, _, v)| v).collect(),
                targets,
                statements,
                built_at,
                skipped: idx.skipped.clone(),
            }),
        },
        domains,
    })
}

/// `t#2` before `t#10`: the id's trailing number compared as a number.
fn natural(id: &str) -> (String, u64, String) {
    match id.rsplit_once('#') {
        // a repaired statement (`#3~r0`) sorts after the number it was repaired from
        Some((head, n)) => {
            let (num, rest) = n.split_once('~').unwrap_or((n, ""));
            (
                head.to_string(),
                num.parse().unwrap_or(u64::MAX),
                rest.to_string(),
            )
        }
        None => (id.to_string(), 0, String::new()),
    }
}

/// A ruling request (§4, R17). Exactly one of `statement` / `target`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RulingRequest {
    #[serde(default)]
    pub statement: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
    /// The `statement_key` the client read (statement subjects).
    #[serde(default)]
    pub expect_key: Option<String>,
    /// The `target_hash` the client read (both subjects).
    pub expect_target_hash: String,
    pub action: String,
    #[serde(default)]
    pub note: Option<String>,
    pub by: String,
}

/// A refusal of [`write_ruling`]: a stable code and the words a person reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RulingRefusal {
    pub code: &'static str,
    pub message: String,
}

fn refuse(code: &'static str, message: impl Into<String>) -> RulingRefusal {
    RulingRefusal {
        code,
        message: message.into(),
    }
}

/// Check a ruling against the store and the checkout, and write it (§4). Writes nothing on a
/// refusal. A later ruling on the same subject replaces the earlier one (the latest row wins).
pub fn write_ruling(
    store: &mut dyn wicked_apps_core::GraphStore,
    repo_root: &Path,
    req: &RulingRequest,
    at: i64,
) -> anyhow::Result<Result<Value, RulingRefusal>> {
    let idx = ExtractIndex::load(&*store as &dyn GraphRead, Some(repo_root))?;
    if !matches!(req.action.as_str(), "confirm" | "reject") {
        return Ok(Err(refuse(
            "bad_request",
            "the action is confirm or reject",
        )));
    }
    if req.by.trim().is_empty() {
        return Ok(Err(refuse("bad_request", "a ruling names who made it")));
    }
    let note = req.note.as_deref().map(str::trim).filter(|n| !n.is_empty());
    if req.action == "reject" && note.is_none() {
        return Ok(Err(refuse(
            "note_required",
            "a reject says what is wrong, so the next run can use it",
        )));
    }
    let (subject, id, symbol, value) = match (&req.statement, &req.target) {
        (Some(s), None) => {
            let Some(st) = idx.statements.get(s) else {
                return Ok(Err(refuse("not_found", format!("no live statement {s}"))));
            };
            if st.effective == Effective::Stale {
                return Ok(Err(refuse(
                    "stale_code",
                    "the code behind this statement changed; the next run checks it again",
                )));
            }
            if st.raw != "open" {
                return Ok(Err(refuse(
                    "not_open",
                    "only an open statement or an unaccounted target takes a ruling",
                )));
            }
            if req.expect_key.as_deref() != Some(st.key.as_str()) {
                return Ok(Err(refuse(
                    "changed",
                    "the statement changed since you read it",
                )));
            }
            let th = str_of(&st.row.value, "target_hash").unwrap_or_default();
            if req.expect_target_hash != th {
                return Ok(Err(refuse(
                    "changed",
                    "the code behind it changed since you read it",
                )));
            }
            (
                "statement",
                s.clone(),
                st.row.symbol.clone(),
                json!({"statement_key": st.key, "target_hash": th}),
            )
        }
        (None, Some(t)) => {
            let Some(tg) = idx.targets.get(t) else {
                return Ok(Err(refuse("not_found", format!("no live target {t}"))));
            };
            if !tg.fresh {
                return Ok(Err(refuse(
                    "stale_code",
                    "the code of this target changed; the next run checks it again",
                )));
            }
            let unaccounted = tg
                .row
                .value
                .get("state")
                .and_then(|s| s.get("unaccounted"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !unaccounted {
                return Ok(Err(refuse(
                    "not_open",
                    "only an open statement or an unaccounted target takes a ruling",
                )));
            }
            let th = str_of(&tg.row.value, "target_hash").unwrap_or_default();
            if req.expect_target_hash != th {
                return Ok(Err(refuse(
                    "changed",
                    "the code behind it changed since you read it",
                )));
            }
            (
                "target",
                t.clone(),
                tg.row.symbol.clone(),
                json!({"target_hash": th}),
            )
        }
        _ => {
            return Ok(Err(refuse(
                "bad_request",
                "name exactly one statement or one target",
            )))
        }
    };
    let mut ruling = value;
    ruling["subject"] = json!(subject);
    ruling["id"] = json!(id);
    ruling["action"] = json!(req.action);
    ruling["by"] = json!(req.by.trim());
    ruling["at"] = json!(at);
    if let Some(n) = note {
        ruling["note"] = json!(n);
    }
    let mut a = wicked_estate_core::Annotation::new(
        RULING_ANN,
        format!("{RULING_ANN}:{subject}:{id}"),
        ruling.to_string(),
    );
    a.author = RULING_AUTHOR.into();
    // One row per subject: the earlier ruling goes in the same transaction the new one is written
    // in, so the live ruling never depends on how rows with equal timestamps are ordered.
    let sym = wicked_apps_core::SymbolId::from(symbol.as_str());
    let key = a.key.clone();
    store.begin_batch()?;
    store.delete_annotations(&sym, Some(RULING_ANN), &key)?;
    store.annotate(&sym, a)?;
    store.commit_batch()?;
    Ok(Ok(ruling))
}

/// What a v3 build did, for the line a command prints.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BuildOutcome {
    pub generation: u64,
    pub nodes: usize,
    pub edges: usize,
    pub requirements: usize,
    pub accounting: Accounting,
}

/// A 1.2.0 model assembled from the store's v3 rows and numbered against one build generation,
/// not yet persisted ([`assemble_build`], then [`persist_build`]).
#[derive(Debug, Clone)]
pub struct AssembledBuild {
    model: crate::domain_model::DomainModel,
    generation: u64,
}

impl AssembledBuild {
    /// The model as it will be persisted.
    pub fn model(&self) -> &crate::domain_model::DomainModel {
        &self.model
    }
}

/// Build the 1.2.0 model from the store's v3 rows and keep the ids of requirements still present.
/// The first half of [`build_and_persist`].
pub fn assemble_build(
    read: &dyn GraphRead,
    repo_root: &Path,
    built_at: i64,
) -> anyhow::Result<AssembledBuild> {
    // The generation is read BEFORE the rows: a build that commits while this one assembles its
    // model moves it, and the persist then refuses instead of writing the older model over it.
    let before = crate::domain_model::build_generation(read)?;
    let mut model = build_extract_model(read, repo_root, built_at)?;
    let generation = crate::domain_model::stabilize_requirement_ids_with(read, &mut model, false)?;
    anyhow::ensure!(
        generation == before,
        "another build of this domain model committed while this one was assembled; build again"
    );
    Ok(AssembledBuild { model, generation })
}

/// Persist an [`assemble_build`] model into the same store: refused, nothing written, when another
/// build committed after it was numbered. The second half of [`build_and_persist`].
pub fn persist_build(
    store: &mut dyn wicked_apps_core::GraphStore,
    build: AssembledBuild,
) -> anyhow::Result<(crate::domain_model::DomainModel, BuildOutcome)> {
    let AssembledBuild { model, generation } = build;
    let (nodes, edges) =
        crate::domain_model::persist_domain_model_expecting(store, &model, Some(generation))?;
    let outcome = BuildOutcome {
        generation: generation + 1,
        nodes,
        edges,
        requirements: model.domains.values().map(|d| d.requirements.len()).sum(),
        accounting: model
            .metadata
            .extract
            .as_ref()
            .map(|e| e.accounting)
            .unwrap_or_default(),
    };
    Ok((model, outcome))
}

/// Build the 1.2.0 model from the store's v3 rows, keep the ids of requirements still present,
/// and persist it into the same store (refused, nothing written, when another build committed in
/// between). The one build path of `domain-graph`, `domain-ruling` and core-ts.
pub fn build_and_persist(
    store: &mut dyn wicked_apps_core::GraphStore,
    repo_root: &Path,
    built_at: i64,
) -> anyhow::Result<(crate::domain_model::DomainModel, BuildOutcome)> {
    let build = assemble_build(&*store, repo_root, built_at)?;
    persist_build(store, build)
}

/// Unix seconds now (a build's `built_at`, a ruling's `at`).
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (core#1955 R2-2) An old-loop `business_rule` row (no `contract`, either key spelling) counts
    /// the 1.1.0 way in a store the old loop alone wrote (a stored 1.1.0 graph's own rules; the
    /// store-level cases are in `tests/domain_model_v3.rs`), and is not counted in a v3 store, as
    /// the 1.2.0 model skips it, even one with no live v3 row left. A v3 tombstone is never
    /// counted. Fails with the old `Legacy` arms.
    #[test]
    fn an_old_loop_rule_counts_the_old_way_only_outside_a_v3_store() {
        let old_loop = [
            ("r", "amount > 0".to_string()),
            ("business_rule:r", json!({"rule": "amount > 0"}).to_string()),
        ];
        let tombstone = json!({"status": RETIRED}).to_string();
        let old = ExtractIndex::default();
        assert!(!old.holds_v3);
        for (key, value) in &old_loop {
            assert_eq!(old.rule_counts(key, value), RuleCount::Legacy, "{key}");
        }
        assert_eq!(old.rule_counts("r", &tombstone), RuleCount::Ignored);
        let v3 = ExtractIndex {
            holds_v3: true,
            ..ExtractIndex::default()
        };
        assert!(v3.is_empty(), "no live v3 row");
        for (key, value) in &old_loop {
            assert_eq!(v3.rule_counts(key, value), RuleCount::Ignored, "{key}");
        }
        assert_eq!(v3.rule_counts("r", &tombstone), RuleCount::Ignored);
    }
}
