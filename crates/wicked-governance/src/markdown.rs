//! MarkdownAdapter — governed docs as a conformance-rule source (AW-3 / arch-R1).
//!
//! ONE parse convention, no second parse path: this adapter only *reads* markdown and emits the
//! same raw JSON bundle shape the JSON [`crate::FilesystemAdapter`] emits; every
//! [`crate::ConformanceRule`] is still materialized exclusively by [`crate::normalize_bundle`]
//! (INV-C1..C4 fail-closed invariants). A malformed doc fails LOUD **per file, with path and
//! reason** — never a silent skip, because a silently-dropped doc reads as "no rules" and would
//! fail governance OPEN.
//!
//! ## The convention
//!
//! A rule doc is a `*.md` file that OPENS with a YAML frontmatter fence. A `.md` file without a
//! leading `---` fence is not a rule doc and is not claimed (uningested-doc reporting is AW-10
//! `rules drift`); a file WITH a fence is claimed, and any malformation from there on is an error.
//!
//! ```markdown
//! ---
//! id: agent-behavior            # required — doc identity (any non-empty string)
//! title: Agent behavior rules   # required
//! status: active                # optional: active|draft|superseded|retired (default active;
//!                               #   non-active docs mint rules with retired=true — parsed,
//!                               #   preserved, withdrawn from recall)
//! enforcement_class: guidance   # optional: policy|validator|guidance (vocabulary validated
//!                               #   here; typing into Policy/validator lanes is AW-7)
//! applies_to: [plan, build]     # optional list
//! scope: wiki:architecture      # optional
//! supersedes: [old-doc-id]      # optional list
//! domain: agent-behavior        # optional — RuleSet parent (grouping is AW-9/AW-13)
//! confidence: 0.9               # optional, in [0,1]; default 1.0; applies to every rule
//! targets:                      # optional — wildcard facets applied to every rule in the doc
//!   language: rust
//!   layer: service
//!   framework: axum
//! ---
//!
//! # Agent behavior rules
//!
//! Prose is fine anywhere outside the Rules section (a doc with NO `## Rules` section is a valid
//! doc-only ingest: zero rules, no error).
//!
//! ## Rules
//!
//! - `PAT-001` (error): Never use `printf` without `%s`.
//! - `POL-002` (critical): All writes go through the single-writer actor,
//!   continuation lines are indented by two or more spaces.
//! ```
//!
//! Each rule item is `- <ID> (<severity>): <statement>` — id matches INV-C1's
//! `^(PAT|POL)-[0-9]{3,6}$` (backticks optional), `rule_type` is DERIVED from the id prefix (the
//! convention makes the prefix the one spelling of the type; INV-C1 requires they agree anyway),
//! severity is one of `info|warn|error|critical`. Anything else inside the Rules section that is
//! not a blank line or an indented continuation is an error. `provenance.ref` is
//! `<root-relative path>@<git blob sha>#<RULE-ID>` (forward slashes on every platform; the sha is
//! the doc's content digest per [`crate::provenance::git_blob_sha1`] — equal to
//! `git hash-object <file>` — which is what lets `rules drift` detect a doc that changed since
//! ingest, AW-10 / arch-R7); `provenance.source_kinds` is `["doc"]`; `provenance.source` is
//! stamped `"markdown"` by the ingest, never by the doc.
//!
//! The frontmatter grammar is a deliberately STRICT YAML subset — top-level `key: value` entries
//! from the known key set, quoted or bare scalars, `[a, b]` flow lists or `- item` block lists,
//! one nested map (`targets`), full-line `#` comments. Unknown keys, anchors, multi-line scalars,
//! and everything else fail loud: a typo'd key must surface, not silently drop a field.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::ingest::SourceAdapter;

// The severity vocabulary (`info|warn|error|critical`, the conformance-rules wire enum) is
// enforced by the rule-item regex in `parse_rules_section`.
/// Doc `status` vocabulary. Anything but `active` mints rules with `retired = true`.
const STATUSES: [&str; 4] = ["active", "draft", "superseded", "retired"];
/// Doc `enforcement_class` vocabulary (arch-R4; the class→lane typing itself lands in AW-7).
const ENFORCEMENT_CLASSES: [&str; 3] = ["policy", "validator", "guidance"];
/// Frontmatter keys the convention knows. Anything else fails loud (a typo must surface).
const KNOWN_KEYS: [&str; 10] = [
    "id",
    "title",
    "status",
    "enforcement_class",
    "applies_to",
    "scope",
    "supersedes",
    "domain",
    "confidence",
    "targets",
];
/// Keys allowed under `targets:` (the wildcard facets of [`crate::Targets`]).
const TARGET_KEYS: [&str; 3] = ["language", "layer", "framework"];

/// Markdown adapter — reads frontmattered `*.md` rule docs under a directory tree (recursive,
/// deterministic order). Emits one raw JSON bundle per doc; ALL rule materialization stays in
/// [`crate::normalize_bundle`].
pub struct MarkdownAdapter {
    root: PathBuf,
}

impl MarkdownAdapter {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Recursively collect `*.md` files under `dir`, sorted per directory (deterministic ingest
    /// order), skipping dot-entries (`.git`, `.wicked-*` scratch). Enumeration faults PROPAGATE —
    /// a silent skip would truncate the rule set and fail governance OPEN.
    fn collect_md(&self, dir: &Path, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
        let entries = std::fs::read_dir(dir)
            .map_err(|e| anyhow::anyhow!("markdown adapter: cannot read {dir:?}: {e}"))?;
        let mut paths: Vec<PathBuf> = Vec::new();
        for entry in entries {
            let entry = entry
                .map_err(|e| anyhow::anyhow!("markdown adapter: cannot enumerate {dir:?}: {e}"))?;
            paths.push(entry.path());
        }
        paths.sort();
        for path in paths {
            let hidden = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'));
            if hidden {
                continue;
            }
            if path.is_dir() {
                self.collect_md(&path, out)?;
            } else if path
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("md"))
            {
                out.push(path);
            }
        }
        Ok(())
    }

    /// The `provenance.ref` path spelling: root-relative, forward slashes on every platform
    /// (cross-platform mandate — refs must compare equal regardless of the ingesting OS).
    fn rel_ref(&self, path: &Path) -> String {
        let rel = path.strip_prefix(&self.root).unwrap_or(path);
        rel.to_string_lossy().replace('\\', "/")
    }
}

impl SourceAdapter for MarkdownAdapter {
    fn name(&self) -> &str {
        "markdown"
    }

    fn fetch(&self) -> anyhow::Result<Vec<serde_json::Value>> {
        let mut files = Vec::new();
        self.collect_md(&self.root, &mut files)?;
        let mut docs = Vec::new();
        for path in files {
            // Read BYTES first: the provenance digest is the git BLOB sha of the raw on-disk
            // content (`git hash-object` — AW-10), so it must be computed before any BOM/CRLF
            // normalization touches the text.
            let bytes = std::fs::read(&path)
                .map_err(|e| anyhow::anyhow!("markdown adapter: cannot read {path:?}: {e}"))?;
            let sha = crate::provenance::git_blob_sha1(&bytes);
            let text = String::from_utf8(bytes).map_err(|e| {
                anyhow::anyhow!("markdown adapter: {path:?} is not valid UTF-8: {e}")
            })?;
            // BOM + CRLF tolerated (cross-platform mandate); the grammar itself stays strict.
            let text = text.trim_start_matches('\u{feff}');
            if !opens_with_fence(text) {
                continue; // no frontmatter fence → not a rule doc (like a stray .txt); AW-10 drift reports these.
            }
            let display = self.rel_ref(&path);
            let doc = parse_doc(text, &display, &sha)
                .map_err(|e| anyhow::anyhow!("markdown adapter: {display}: {e}"))?;
            docs.push(doc);
        }
        Ok(docs)
    }
}

/// A claimed doc opens with a `---` fence as its first non-empty line content.
fn opens_with_fence(text: &str) -> bool {
    text.lines().next().map(|l| l.trim_end()) == Some("---")
}

/// Parsed frontmatter — the doc-level convention fields.
#[derive(Default)]
struct FrontMatter {
    id: Option<String>,
    title: Option<String>,
    status: Option<String>,
    enforcement_class: Option<String>,
    applies_to: Option<Vec<String>>,
    scope: Option<String>,
    supersedes: Option<Vec<String>>,
    domain: Option<String>,
    confidence: Option<f64>,
    targets_language: Option<String>,
    targets_layer: Option<String>,
    targets_framework: Option<String>,
}

/// Parse one claimed doc into the raw bundle `Value` shape `normalize_bundle` consumes:
/// `{ "doc": {…frontmatter…, "path": <ref path>, "sha": <blob sha>}, "rules": [ … ] }`. Fails
/// loud with a line-numbered reason on any malformation.
fn parse_doc(text: &str, ref_path: &str, sha: &str) -> anyhow::Result<serde_json::Value> {
    let lines: Vec<&str> = text.lines().map(|l| l.trim_end_matches('\r')).collect();
    let (fm, body_start) = parse_frontmatter(&lines)?;
    let rules = parse_rules_section(&lines, body_start, &fm, ref_path, sha)?;

    let mut doc_meta = serde_json::Map::new();
    doc_meta.insert("path".into(), ref_path.into());
    doc_meta.insert("sha".into(), sha.into());
    doc_meta.insert("id".into(), fm.id.clone().expect("validated").into());
    doc_meta.insert("title".into(), fm.title.clone().expect("validated").into());
    if let Some(v) = &fm.status {
        doc_meta.insert("status".into(), v.clone().into());
    }
    if let Some(v) = &fm.enforcement_class {
        doc_meta.insert("enforcement_class".into(), v.clone().into());
    }
    if let Some(v) = &fm.scope {
        doc_meta.insert("scope".into(), v.clone().into());
    }
    if let Some(v) = &fm.domain {
        doc_meta.insert("domain".into(), v.clone().into());
    }
    if let Some(v) = &fm.applies_to {
        doc_meta.insert("applies_to".into(), v.clone().into());
    }
    if let Some(v) = &fm.supersedes {
        doc_meta.insert("supersedes".into(), v.clone().into());
    }

    Ok(serde_json::json!({ "doc": doc_meta, "rules": rules }))
}

/// Parse the STRICT frontmatter subset between the opening fence (line 0, pre-checked) and the
/// closing `---`. Returns the frontmatter + the body's first line index.
fn parse_frontmatter(lines: &[&str]) -> anyhow::Result<(FrontMatter, usize)> {
    let close = lines[1..]
        .iter()
        .position(|l| l.trim_end() == "---")
        .map(|i| i + 1)
        .ok_or_else(|| {
            anyhow::anyhow!("frontmatter opened at line 1 but never closed (no `---`)")
        })?;

    let mut fm = FrontMatter::default();
    let mut seen: HashSet<String> = HashSet::new();
    let mut i = 1;
    while i < close {
        let raw = lines[i];
        let line_no = i + 1;
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }
        if raw.starts_with(' ') || raw.starts_with('\t') {
            anyhow::bail!(
                "frontmatter line {line_no}: unexpected indented line {trimmed:?} (block values \
                 belong to `applies_to`/`supersedes`/`targets` and are consumed with their key)"
            );
        }
        let Some((key, rest)) = raw.split_once(':') else {
            anyhow::bail!("frontmatter line {line_no}: expected `key: value`, got {raw:?}");
        };
        let key = key.trim();
        if !KNOWN_KEYS.contains(&key) {
            anyhow::bail!(
                "frontmatter line {line_no}: unknown key {key:?} (known: {KNOWN_KEYS:?}) — \
                 refusing to silently drop a field"
            );
        }
        if !seen.insert(key.to_string()) {
            anyhow::bail!("frontmatter line {line_no}: duplicate key {key:?}");
        }
        let rest = rest.trim();
        match key {
            "id" | "title" | "status" | "enforcement_class" | "scope" | "domain" => {
                let v = parse_scalar(rest, line_no, key)?;
                match key {
                    "id" => fm.id = Some(v),
                    "title" => fm.title = Some(v),
                    "status" => fm.status = Some(v),
                    "enforcement_class" => fm.enforcement_class = Some(v),
                    "scope" => fm.scope = Some(v),
                    _ => fm.domain = Some(v),
                }
                i += 1;
            }
            "confidence" => {
                let v = parse_scalar(rest, line_no, key)?;
                let c: f64 = v.parse().map_err(|_| {
                    anyhow::anyhow!("frontmatter line {line_no}: confidence {v:?} is not a number")
                })?;
                if !(0.0..=1.0).contains(&c) {
                    anyhow::bail!(
                        "frontmatter line {line_no}: confidence {c} outside [0,1] (INV-C2)"
                    );
                }
                fm.confidence = Some(c);
                i += 1;
            }
            "applies_to" | "supersedes" => {
                let (list, consumed) = parse_list(lines, i, close, rest, key)?;
                if key == "applies_to" {
                    fm.applies_to = Some(list);
                } else {
                    fm.supersedes = Some(list);
                }
                i = consumed;
            }
            "targets" => {
                if !rest.is_empty() {
                    anyhow::bail!(
                        "frontmatter line {line_no}: `targets` takes an indented map on the \
                         following lines, not an inline value ({rest:?})"
                    );
                }
                i += 1;
                let mut any = false;
                while i < close {
                    let l = lines[i];
                    if l.trim().is_empty() {
                        i += 1;
                        continue;
                    }
                    if !l.starts_with("  ") {
                        break; // next top-level key
                    }
                    let tline_no = i + 1;
                    let t = l.trim();
                    let Some((tk, tv)) = t.split_once(':') else {
                        anyhow::bail!(
                            "frontmatter line {tline_no}: expected `<facet>: value` under targets, \
                             got {t:?}"
                        );
                    };
                    let tk = tk.trim();
                    if !TARGET_KEYS.contains(&tk) {
                        anyhow::bail!(
                            "frontmatter line {tline_no}: unknown targets facet {tk:?} \
                             (known: {TARGET_KEYS:?})"
                        );
                    }
                    let tv = parse_scalar(tv.trim(), tline_no, tk)?;
                    match tk {
                        "language" => fm.targets_language = Some(tv),
                        "layer" => fm.targets_layer = Some(tv),
                        _ => fm.targets_framework = Some(tv),
                    }
                    any = true;
                    i += 1;
                }
                if !any {
                    anyhow::bail!(
                        "frontmatter line {line_no}: `targets:` opened a map but no indented \
                         `language`/`layer`/`framework` entries follow"
                    );
                }
            }
            _ => unreachable!("key vetted against KNOWN_KEYS"),
        }
    }

    // Required doc identity — fail loud, never fabricate.
    if fm.id.as_deref().is_none_or(str::is_empty) {
        anyhow::bail!("frontmatter is missing required `id` (doc identity; never fabricated)");
    }
    if fm.title.as_deref().is_none_or(str::is_empty) {
        anyhow::bail!("frontmatter is missing required `title` (never fabricated)");
    }
    if let Some(s) = &fm.status {
        if !STATUSES.contains(&s.as_str()) {
            anyhow::bail!("frontmatter `status` {s:?} is not one of {STATUSES:?}");
        }
    }
    if let Some(c) = &fm.enforcement_class {
        if !ENFORCEMENT_CLASSES.contains(&c.as_str()) {
            anyhow::bail!(
                "frontmatter `enforcement_class` {c:?} is not one of {ENFORCEMENT_CLASSES:?}"
            );
        }
    }
    Ok((fm, close + 1))
}

/// A scalar value: bare (trimmed) or wrapped in one pair of matching quotes. Never empty.
fn parse_scalar(raw: &str, line_no: usize, key: &str) -> anyhow::Result<String> {
    let v = raw.trim();
    let v = if (v.starts_with('"') && v.ends_with('"') && v.len() >= 2)
        || (v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2)
    {
        &v[1..v.len() - 1]
    } else {
        v
    };
    if v.is_empty() {
        anyhow::bail!("frontmatter line {line_no}: `{key}` has an empty value");
    }
    Ok(v.to_string())
}

/// A list value: `[a, b]` flow form on the key line, or `- item` block lines below it. Returns
/// the items + the line index AFTER the consumed lines.
fn parse_list(
    lines: &[&str],
    key_ix: usize,
    close: usize,
    rest: &str,
    key: &str,
) -> anyhow::Result<(Vec<String>, usize)> {
    let line_no = key_ix + 1;
    if !rest.is_empty() {
        // Flow list `[a, b]`.
        let inner = rest
            .strip_prefix('[')
            .and_then(|r| r.strip_suffix(']'))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "frontmatter line {line_no}: `{key}` must be a `[a, b]` flow list or `- item` \
                     block list, got {rest:?}"
                )
            })?;
        let mut items = Vec::new();
        for part in inner.split(',') {
            if part.trim().is_empty() {
                continue; // tolerate `[]` and a trailing comma
            }
            items.push(parse_scalar(part, line_no, key)?);
        }
        return Ok((items, key_ix + 1));
    }
    // Block list.
    let mut items = Vec::new();
    let mut i = key_ix + 1;
    while i < close {
        let l = lines[i];
        let t = l.trim();
        if t.is_empty() {
            i += 1;
            continue;
        }
        if !l.starts_with(' ') && !t.starts_with('-') {
            break; // next top-level key
        }
        let Some(item) = t.strip_prefix("- ") else {
            anyhow::bail!(
                "frontmatter line {}: expected `- item` under `{key}`, got {t:?}",
                i + 1
            );
        };
        items.push(parse_scalar(item, i + 1, key)?);
        i += 1;
    }
    if items.is_empty() {
        anyhow::bail!(
            "frontmatter line {line_no}: `{key}:` opened a block list but no `- item` lines follow"
        );
    }
    Ok((items, i))
}

/// Find and parse the single `## Rules` section into raw rule JSON objects. A doc with NO Rules
/// section is a valid doc-only ingest (zero rules). Inside the section only rule items, their
/// indented continuations, and blank lines are legal — anything else fails loud (line-numbered),
/// never a silent skip.
fn parse_rules_section(
    lines: &[&str],
    body_start: usize,
    fm: &FrontMatter,
    ref_path: &str,
    sha: &str,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let is_rules_heading = |l: &str| l.trim_end() == "## Rules";
    let mut heading_ix: Option<usize> = None;
    for (ix, l) in lines.iter().enumerate().skip(body_start) {
        if is_rules_heading(l) {
            if heading_ix.is_some() {
                anyhow::bail!(
                    "line {}: a second `## Rules` section — the convention allows exactly one",
                    ix + 1
                );
            }
            heading_ix = Some(ix);
        }
    }
    let Some(start) = heading_ix else {
        return Ok(Vec::new()); // doc-only ingest: valid, zero rules
    };

    // The rule item shape: `- <ID> (<severity>): <statement>` (backticks around the id optional).
    let item_re = regex::Regex::new(
        r"^[-*]\s+`?((?:PAT|POL)-[0-9]{3,6})`?\s*\((info|warn|error|critical)\)\s*:\s*(.*)$",
    )
    .expect("static regex compiles");

    let retired = fm.status.as_deref().is_some_and(|s| s != "active");
    let confidence = fm.confidence.unwrap_or(1.0);
    let mut targets = serde_json::Map::new();
    if let Some(v) = &fm.targets_language {
        targets.insert("language".into(), v.clone().into());
    }
    if let Some(v) = &fm.targets_layer {
        targets.insert("layer".into(), v.clone().into());
    }
    if let Some(v) = &fm.targets_framework {
        targets.insert("framework".into(), v.clone().into());
    }

    let mut rules: Vec<serde_json::Value> = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();
    // (id, severity, statement) of the item currently being assembled (continuations may extend it).
    let mut current: Option<(String, String, String)> = None;
    let flush = |current: &mut Option<(String, String, String)>,
                 rules: &mut Vec<serde_json::Value>| {
        if let Some((id, severity, statement)) = current.take() {
            let rule_type = if id.starts_with("PAT-") {
                "pattern"
            } else {
                "policy"
            };
            let mut rule = serde_json::json!({
                "id": id,
                "rule_type": rule_type,
                "statement": statement,
                "severity": severity,
                "confidence": confidence,
                // `<path>@<blob sha>#<id>` — the AW-10 digest-bearing ref (crate::provenance).
                "provenance": {
                    "ref": crate::provenance::format_provenance_ref(ref_path, sha, &id),
                    "source_kinds": ["doc"],
                },
            });
            if !targets.is_empty() {
                rule["targets"] = serde_json::Value::Object(targets.clone());
            }
            if retired {
                rule["retired"] = serde_json::Value::Bool(true);
            }
            rules.push(rule);
        }
    };

    for (ix, l) in lines.iter().enumerate().skip(start + 1) {
        let line_no = ix + 1;
        // Section ends at the next `#`/`##` heading.
        if l.starts_with("# ") || l.starts_with("## ") {
            break;
        }
        let t = l.trim();
        if t.is_empty() {
            continue;
        }
        if let Some(caps) = item_re.captures(l) {
            flush(&mut current, &mut rules);
            let id = caps[1].to_string();
            if !seen_ids.insert(id.clone()) {
                anyhow::bail!("line {line_no}: duplicate rule id {id:?} within this doc (INV-C3)");
            }
            current = Some((id, caps[2].to_string(), caps[3].trim().to_string()));
        } else if let (true, Some(cur)) = (l.starts_with("  "), current.as_mut()) {
            // Continuation line: joined with a space onto the open statement. An indented line
            // with NO open item falls through to the fail-loud arm below.
            if !cur.2.is_empty() {
                cur.2.push(' ');
            }
            cur.2.push_str(t);
        } else {
            anyhow::bail!(
                "line {line_no}: unrecognized content in the Rules section: {t:?} — expected \
                 `- <PAT|POL-nnn> (<info|warn|error|critical>): <statement>`, an indented \
                 continuation, or a blank line (refusing to silently skip)"
            );
        }
    }
    flush(&mut current, &mut rules);

    // A rule with an EMPTY statement (even after continuations) is malformed — normalize_bundle
    // would accept the field as present; the convention says a rule states something.
    for r in &rules {
        if r["statement"].as_str().is_some_and(str::is_empty) {
            anyhow::bail!("rule {} has an empty statement", r["id"]);
        }
    }
    Ok(rules)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::ingest_from;

    /// Write `files` under a fresh per-test temp dir; returns the dir.
    fn dir_with(files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wicked-gov-md-{}-{:p}",
            std::process::id(),
            &files[0].0 // distinct per call site via the string's static address
        ));
        let _ = std::fs::remove_dir_all(&dir);
        for (name, content) in files {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        dir
    }

    const WELL_FORMED: &str = "---\n\
        id: agent-behavior\n\
        title: Agent behavior rules\n\
        scope: wiki:architecture\n\
        applies_to: [plan, build]\n\
        confidence: 0.9\n\
        targets:\n  language: rust\n\
        ---\n\n\
        # Agent behavior rules\n\nProse.\n\n\
        ## Rules\n\n\
        - `PAT-001` (error): Never use printf without %s.\n\
        - POL-002 (critical): All writes go through the single-writer actor,\n  \
          joined across lines.\n\n\
        ## Appendix\n\nMore prose.\n";

    #[test]
    fn well_formed_doc_ingests_to_rules_through_normalize_bundle() {
        let dir = dir_with(&[("agent-behavior.md", WELL_FORMED)]);
        let rules = ingest_from(&MarkdownAdapter::new(&dir)).unwrap();
        assert_eq!(rules.len(), 2);
        let pat = &rules[0];
        assert_eq!(pat.id, "PAT-001");
        assert_eq!(pat.rule_type, crate::RuleType::Pattern);
        assert_eq!(pat.severity, crate::ConfSeverity::Error);
        assert_eq!(pat.statement, "Never use printf without %s.");
        assert!((pat.confidence - 0.9).abs() < 1e-6, "doc-level confidence");
        assert_eq!(pat.targets.language.as_deref(), Some("rust"));
        assert_eq!(
            pat.provenance.source, "markdown",
            "ingest stamps the adapter"
        );
        let expected_sha = crate::provenance::git_blob_sha1(WELL_FORMED.as_bytes());
        assert_eq!(
            pat.provenance.reference.as_deref(),
            Some(format!("agent-behavior.md@{expected_sha}#PAT-001").as_str()),
            "ref = root-relative path + content digest (git blob sha, AW-10) + rule anchor"
        );
        assert_eq!(pat.provenance.source_kinds, vec!["doc".to_string()]);
        assert!(!pat.retired);
        let pol = &rules[1];
        assert_eq!(pol.rule_type, crate::RuleType::Policy);
        assert_eq!(
            pol.statement, "All writes go through the single-writer actor, joined across lines.",
            "continuation lines join with a space"
        );
    }

    #[test]
    fn well_formed_doc_registers_as_rule_nodes() {
        crate::events::hermetic_test_spool();
        use wicked_apps_core::{GraphRead, NodeKind, SqliteStore};
        let dir = dir_with(&[("nodes.md", WELL_FORMED)]);
        let rules = ingest_from(&MarkdownAdapter::new(&dir)).unwrap();
        let mut store = SqliteStore::in_memory().unwrap();
        for r in &rules {
            crate::register_rule(&mut store, r).unwrap();
        }
        let query = wicked_estate_core::SymbolQuery {
            kinds: vec![NodeKind::Rule],
            ..Default::default()
        };
        assert_eq!(
            store.find_symbols(&query).unwrap().len(),
            2,
            "frontmattered doc → native NodeKind::Rule nodes"
        );
        let recalled = crate::recall_rules(&store, &crate::RuleQuery::default()).unwrap();
        assert_eq!(recalled.len(), 2);
        assert_eq!(recalled[0].id, "POL-002", "critical orders before error");
    }

    #[test]
    fn malformed_frontmatter_fails_loud_with_path_and_reason() {
        // Unclosed fence.
        let dir = dir_with(&[("bad-fence.md", "---\nid: x\ntitle: y\n\n# no close\n")]);
        let err = ingest_from(&MarkdownAdapter::new(&dir))
            .unwrap_err()
            .to_string();
        assert!(err.contains("bad-fence.md"), "names the file: {err}");
        assert!(err.contains("never closed"), "names the reason: {err}");

        // Unknown key (typo) — refuses to silently drop a field.
        let dir = dir_with(&[("typo-key.md", "---\nid: x\ntitle: y\nseverty: error\n---\n")]);
        let err = ingest_from(&MarkdownAdapter::new(&dir))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("typo-key.md") && err.contains("severty"),
            "{err}"
        );

        // Missing required id.
        let dir = dir_with(&[("no-id.md", "---\ntitle: y\n---\n")]);
        let err = ingest_from(&MarkdownAdapter::new(&dir))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no-id.md") && err.contains("`id`"), "{err}");

        // Out-of-range confidence (INV-C2 surfaced at parse, with the path).
        let dir = dir_with(&[("conf.md", "---\nid: x\ntitle: y\nconfidence: 1.5\n---\n")]);
        let err = ingest_from(&MarkdownAdapter::new(&dir))
            .unwrap_err()
            .to_string();
        assert!(err.contains("conf.md") && err.contains("INV-C2"), "{err}");
    }

    #[test]
    fn malformed_rule_line_fails_loud_never_skipped() {
        let dir = dir_with(&[(
            "bad-rule.md",
            "---\nid: x\ntitle: y\n---\n\n## Rules\n\n- PAT-001 missing severity and colon\n",
        )]);
        let err = ingest_from(&MarkdownAdapter::new(&dir))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("bad-rule.md") && err.contains("unrecognized content"),
            "a non-conforming rule line must fail the FILE, not be skipped: {err}"
        );
    }

    #[test]
    fn duplicate_rule_id_within_a_doc_fails_loud() {
        let dir = dir_with(&[(
            "dup.md",
            "---\nid: x\ntitle: y\n---\n\n## Rules\n\n- PAT-001 (error): a\n- PAT-001 (warn): b\n",
        )]);
        let err = ingest_from(&MarkdownAdapter::new(&dir))
            .unwrap_err()
            .to_string();
        assert!(err.contains("dup.md") && err.contains("INV-C3"), "{err}");
    }

    #[test]
    fn empty_rules_section_is_a_valid_doc_only_ingest() {
        // No `## Rules` at all.
        let dir = dir_with(&[(
            "doc-only.md",
            "---\nid: overview\ntitle: Overview\n---\n\n# Overview\n\nProse only.\n",
        )]);
        assert_eq!(ingest_from(&MarkdownAdapter::new(&dir)).unwrap().len(), 0);

        // `## Rules` present but empty.
        let dir = dir_with(&[(
            "empty-rules.md",
            "---\nid: empty\ntitle: Empty\n---\n\n## Rules\n\n## Next\n",
        )]);
        assert_eq!(
            ingest_from(&MarkdownAdapter::new(&dir)).unwrap().len(),
            0,
            "an empty Rules section is valid (doc-only), not an error"
        );
    }

    #[test]
    fn non_active_status_mints_retired_rules() {
        let dir = dir_with(&[(
            "superseded.md",
            "---\nid: old\ntitle: Old doctrine\nstatus: superseded\n---\n\n## Rules\n\n- POL-010 (error): old rule.\n",
        )]);
        let rules = ingest_from(&MarkdownAdapter::new(&dir)).unwrap();
        assert_eq!(rules.len(), 1);
        assert!(
            rules[0].retired,
            "non-active doc → rules preserved but withdrawn from recall"
        );
    }

    #[test]
    fn plain_markdown_without_frontmatter_is_not_claimed() {
        let dir = dir_with(&[
            ("README.md", "# Just a readme\n\nNo frontmatter fence.\n"),
            (
                "real.md",
                "---\nid: r\ntitle: R\n---\n\n## Rules\n\n- PAT-001 (info): s.\n",
            ),
        ]);
        let rules = ingest_from(&MarkdownAdapter::new(&dir)).unwrap();
        assert_eq!(
            rules.len(),
            1,
            "README skipped as a non-doc, real doc ingested"
        );
    }

    #[test]
    fn discovery_is_recursive_and_crlf_tolerant() {
        let crlf = "---\r\nid: win\r\ntitle: Windows doc\r\n---\r\n\r\n## Rules\r\n\r\n- PAT-002 (warn): crlf statement.\r\n";
        let dir = dir_with(&[("nested/deeper/win.md", crlf)]);
        let rules = ingest_from(&MarkdownAdapter::new(&dir)).unwrap();
        assert_eq!(rules.len(), 1);
        let expected_sha = crate::provenance::git_blob_sha1(crlf.as_bytes());
        assert_eq!(
            rules[0].provenance.reference.as_deref(),
            Some(format!("nested/deeper/win.md@{expected_sha}#PAT-002").as_str()),
            "ref uses forward slashes regardless of platform, digest over the RAW bytes"
        );
        let parsed = crate::provenance::parse_provenance_ref(
            rules[0].provenance.reference.as_deref().unwrap(),
        );
        assert_eq!(parsed.path, "nested/deeper/win.md");
        assert_eq!(parsed.sha.as_deref(), Some(expected_sha.as_str()));
        assert_eq!(parsed.anchor.as_deref(), Some("PAT-002"));
    }
}
