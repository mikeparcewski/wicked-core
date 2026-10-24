//! FLOOR GUIDE (DES-BUILD-GUIDE, contract v1) — the per-repo build/test guide the repo-checks
//! floor interprets. The guide is DATA (`.wicked/build-guide.json`), discovered at onboarding and
//! checked piece by piece by `wicked-core floor probe` ([`crate::floor_probe`]); this module reads
//! it, decides whether a check's pieces may be relied on, and turns its pieces into argv and parsed
//! test ids. It knows no runner: every runner fact (how to list, run one test, deselect, read a
//! result) is a guide piece, and a piece the probes did not pass is never relied on.
//!
//! The floor reads the guide from the BASE tree only, so a head edit to the guide never applies to
//! its own run. Its parsed results only nominate candidates; exit codes decide (DES-FLOOR-V2).

use std::collections::BTreeMap;
use std::path::Path;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// Where a repository keeps its guide.
pub const GUIDE_PATH: &str = ".wicked/build-guide.json";
/// The probes every check needs, all `pass`, before its exclusion is relied on (plus `affected`
/// when the check has that piece).
pub const REQUIRED_PROBES: [&str; 8] = [
    "list", "run", "complete", "ids", "alone", "deselect", "mutation", "multi",
];

/// A repository's build/test guide (contract v1).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct BuildGuide {
    pub version: u32,
    /// The repo-relative files the guide was derived from: a base change to one makes it stale.
    #[serde(default)]
    pub derived_from: Vec<String>,
    /// Required: a guide that misspells or drops it does not parse, and detection decides.
    pub checks: Vec<GuideCheck>,
}

/// One check of the guide.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct GuideCheck {
    pub name: String,
    #[serde(default = "dot")]
    pub cwd: String,
    #[serde(default)]
    pub stage: Vec<String>,
    /// The exact argv: exclusion happens only for a floor check whose argv equals it.
    pub run: Vec<String>,
    /// Exit codes that mean "tests failed" (any other non-zero exit: a crash, could not run).
    #[serde(default)]
    pub failed_exit: Vec<i32>,
    /// How a test check's results are read; a gate check (no `list`) has none.
    #[serde(default, skip_serializing_if = "Results::is_unset")]
    pub results: Results,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parser: Option<Parser>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list: Option<ListPiece>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alone: Option<RunPiece>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deselect: Option<Deselect>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affected: Option<RunPiece>,
    /// The runner's version command: `floor probe` records its first output line as
    /// `verified.runnerVersion`, and the floor relies on the check only while it still prints that.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<RunPiece>,
    /// Globs: a head file the change touched that matches one disables exclusion for this check.
    #[serde(default)]
    pub invalidated_by: Vec<String>,
    #[serde(default)]
    pub probes: Probes,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified: Option<Verified>,
}

fn dot() -> String {
    ".".into()
}

impl Results {
    fn is_unset(&self) -> bool {
        *self == Results::default()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct Results {
    /// `text` | `junit` (v1); `json-lines` | `tap` are reserved.
    pub kind: String,
    #[serde(default)]
    pub how: How,
    /// (junit) The testcase attributes whose values, joined by `::`, form an id (default
    /// `["classname", "name"]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub id: Vec<String>,
    /// (junit) What joins the `id` attributes (default `::`), so the ids can be the list's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_join: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct How {
    /// Appended to every argv whose results the floor reads (the base run, the probes' runs).
    #[serde(default)]
    pub argv_append: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// The results file (relative to `cwd`), when the results are not on stdout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Capture stderr into the same stream as stdout, in order (a runner that names its test
    /// binaries on stderr and their results on stdout).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub merge: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Parser {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    /// How many tests the run executed: on each matching line every named group's integer is
    /// added (say passed, failed and ignored). Without it a text guide cannot prove its list
    /// complete, and is never relied on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<String>,
    #[serde(default)]
    pub failed: Vec<String>,
    #[serde(default)]
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Block {
    pub start: String,
    pub item: String,
    pub end: String,
    /// `last-before-end` (default) | `all`.
    #[serde(rename = "use", default = "last_before_end")]
    pub use_: String,
}

fn last_before_end() -> String {
    "last-before-end".into()
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListPiece {
    pub run: Vec<String>,
    pub item: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    /// As [`How::merge`], for the list run.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub merge: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunPiece {
    pub run: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Deselect {
    pub append: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Probes {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mutation: Option<Mutation>,
    /// A SECOND deliberately failing test: with both written, both are parsed and deselecting
    /// both runs clean (a deselect that breaks with two ids, a summary cut at a wrapped line).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mutation2: Option<Mutation>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct Mutation {
    /// Relative to the check's `cwd`.
    pub file: String,
    pub content: String,
    pub expect_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_q: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct Verified {
    #[serde(default)]
    pub base_sha: String,
    #[serde(default)]
    pub runner_version: String,
    #[serde(default)]
    pub at: String,
    #[serde(default)]
    pub results: BTreeMap<String, String>,
}

/// One test: its qualifier scope (`q`, e.g. a test binary; empty when the list has none) and id.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TestId {
    pub q: String,
    pub id: String,
}

/// The guide's state for one floor check, on the wire (`decision.guide`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuideStatus {
    /// The base's guide has a check whose argv and cwd are this check's.
    pub present: bool,
    /// Its pieces may be relied on for this head (probes passed, verified on an ancestor, not
    /// stale, not invalidated by the change).
    pub usable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The base tree's guide, read with git from `base` — never from the head's worktree. `Ok(None)`
/// when the base has none; `Err` when it has one that does not parse.
pub(crate) fn load_base(
    worktree: &Path,
    git_dir: &Path,
    base: &str,
) -> Result<Option<BuildGuide>, String> {
    let env: [(&str, &Path); 1] = [("GIT_DIR", git_dir)];
    let spec = format!("{base}:{GUIDE_PATH}");
    if crate::worktree_guard::git(worktree, &["cat-file", "-e", &spec], &env).is_err() {
        return Ok(None);
    }
    let raw = crate::worktree_guard::git_string(worktree, &["cat-file", "blob", &spec], &env)
        .map_err(|e| format!("the base's `{GUIDE_PATH}` could not be read: {e}"))?;
    parse(&raw)
        .map(Some)
        .map_err(|e| format!("the base's `{GUIDE_PATH}` {e}"))
}

/// A relative directory inside the repo: `.`/empty, or segments none of which is empty, `.` or
/// `..`, with no `\\` and no `:` (no Windows drive, `C:x` included).
fn cwd_ok(cwd: &str) -> bool {
    cwd.is_empty()
        || cwd == "."
        || !(Path::new(cwd).is_absolute()
            || cwd.starts_with('/')
            || cwd.contains('\\')
            || cwd.contains(':')
            || cwd
                .split('/')
                .any(|s| s.is_empty() || s == "." || s == ".."))
}

/// A guide's text, parsed and checked against the parts of the schema the floor relies on: version
/// 1, every `run` non-empty, `stage` values `creator` | `verify`, a test check (one with a `list`)
/// naming its `failedExit` (never 0), a `cwd` inside the repo, and a `deselect` whose
/// `{ids:re|}` is once-only and never beside `{q}`. Anything else does not parse (detection
/// decides).
pub(crate) fn parse(raw: &str) -> Result<BuildGuide, String> {
    let guide: BuildGuide =
        serde_json::from_str(raw).map_err(|e| format!("does not parse: {e}"))?;
    if guide.version != 1 {
        return Err(format!(
            "is version {} (this engine reads 1)",
            guide.version
        ));
    }
    for c in &guide.checks {
        let tokens = c.deselect.as_ref().map_or(&[][..], |d| d.append.as_slice());
        let (first, last) = per_id_range(tokens);
        let has = |t: &[String], s: &str| t.iter().any(|x| x.contains(s));
        let bad = if c.run.is_empty() {
            Some("an empty `run`")
        } else if c.stage.iter().any(|s| s != "creator" && s != "verify") {
            Some("a `stage` other than `creator` / `verify`")
        } else if c.list.is_some() && c.failed_exit.is_empty() {
            Some("a `list` and no `failedExit`")
        } else if c.failed_exit.contains(&0) {
            Some("`0` in `failedExit` (a failure exits nonzero)")
        } else if !cwd_ok(&c.cwd) {
            Some("a `cwd` that is absolute, climbs (`..`) or has an empty segment")
        } else if has(&tokens[first..last], "{ids:re|}") {
            Some("`{ids:re|}` among the per-id tokens of `deselect.append` (it is once-only)")
        } else if tokens.iter().any(|t| {
            let id = t.contains("{id}") || t.contains("{id:re}");
            t.contains("{q}") != id && has(tokens, "{q}")
        }) {
            Some("`{q}` and `{id}` in different tokens of `deselect.append` (one token must bind an id to its qualifier)")
        } else if has(tokens, "{ids:re|}") && has(tokens, "{q}") {
            Some("`{ids:re|}` and `{q}` in one `deselect.append` (the pattern cannot bind ids to qualifiers)")
        } else {
            None
        };
        if let Some(bad) = bad {
            return Err(format!("is invalid: check `{}` has {bad}", c.name));
        }
    }
    Ok(guide)
}

fn norm_dir(d: &str) -> &str {
    let d = d.trim_end_matches('/');
    if d == "." {
        ""
    } else {
        d.strip_prefix("./").unwrap_or(d)
    }
}

impl BuildGuide {
    /// The checks that run at `stage` (`creator` | `verify`): a check with no `stage` runs at both.
    pub(crate) fn checks_at<'a>(&'a self, stage: &'a str) -> impl Iterator<Item = &'a GuideCheck> {
        self.checks
            .iter()
            .filter(move |c| c.stage.is_empty() || c.stage.iter().any(|s| s == stage))
    }

    /// The guide check whose `run` is exactly `argv` in `dir` — the guide is the allowlist.
    pub(crate) fn check_for(&self, argv: &[String], dir: &str) -> Option<&GuideCheck> {
        self.checks
            .iter()
            .find(|c| c.run == argv && norm_dir(&c.cwd) == norm_dir(dir))
    }
}

/// A compiled parser: each regex applies to one line at a time (RE2 syntax: the `regex` crate).
pub(crate) struct Compiled {
    section: Option<Regex>,
    count: Option<Regex>,
    failed: Vec<Regex>,
    blocks: Vec<(Regex, Regex, Regex, bool)>,
    list_item: Regex,
    list_section: Option<Regex>,
}

fn re(p: &str) -> Result<Regex, String> {
    Regex::new(p).map_err(|e| format!("regex `{p}`: {e}"))
}

impl GuideCheck {
    /// The cwd relative to the repo root (`""` = the root).
    pub(crate) fn dir(&self) -> &str {
        norm_dir(&self.cwd)
    }

    /// Compile every regex the check's pieces name; `Err` names the first that is missing or bad.
    pub(crate) fn compile(&self) -> Result<Compiled, String> {
        let list = self.list.as_ref().ok_or("no `list` piece")?;
        let (section, count, failed, blocks) = match self.results.kind.as_str() {
            "text" => {
                let p = self
                    .parser
                    .as_ref()
                    .ok_or("`results.kind` text with no `parser`")?;
                let blocks = p
                    .blocks
                    .iter()
                    .map(|b| {
                        let all = match b.use_.as_str() {
                            "all" => true,
                            "last-before-end" => false,
                            u => return Err(format!("block `use` `{u}`")),
                        };
                        Ok((re(&b.start)?, re(&b.item)?, re(&b.end)?, all))
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                (
                    p.section.as_deref().map(re).transpose()?,
                    p.count.as_deref().map(re).transpose()?,
                    p.failed.iter().map(|f| re(f)).collect::<Result<_, _>>()?,
                    blocks,
                )
            }
            "junit" => (None, None, Vec::new(), Vec::new()),
            k @ ("json-lines" | "tap") => {
                return Err(format!("`results.kind` `{k}` is reserved (not read in v1)"))
            }
            k => return Err(format!("`results.kind` `{k}`")),
        };
        Ok(Compiled {
            section,
            count,
            failed,
            blocks,
            list_item: re(&list.item)?,
            list_section: list.section.as_deref().map(re).transpose()?,
        })
    }

    /// May the floor rely on this check's pieces against `base` for a change touching `touched`?
    /// Every required probe passed, on a commit that is an ancestor of the base, no file the guide
    /// was derived from changed since, and no touched file matches `invalidatedBy`.
    pub(crate) fn usable(
        &self,
        guide: &BuildGuide,
        worktree: &Path,
        git_dir: &Path,
        base: &str,
        touched: &[String],
    ) -> Result<(), String> {
        self.compile()?;
        for piece in [
            ("alone", self.alone.is_some()),
            ("deselect", self.deselect.is_some()),
        ] {
            if !piece.1 {
                return Err(format!("no `{}` piece", piece.0));
            }
        }
        if self.failed_exit.is_empty() {
            return Err("no `failedExit`".into());
        }
        let v = self
            .verified
            .as_ref()
            .ok_or("never probed (no `verified`)")?;
        let mut probes: Vec<&str> = REQUIRED_PROBES.to_vec();
        if self.affected.is_some() {
            probes.push("affected");
        }
        if self.version.is_some() {
            probes.push("version");
        }
        for p in probes {
            match v.results.get(p).map(String::as_str) {
                Some(r) if passed(r) => {}
                Some(r) => return Err(format!("probe `{p}`: {r}")),
                None => return Err(format!("probe `{p}` never ran")),
            }
        }
        let env: [(&str, &Path); 1] = [("GIT_DIR", git_dir)];
        let hex = !v.base_sha.is_empty() && v.base_sha.bytes().all(|b| b.is_ascii_hexdigit());
        if !hex
            || crate::worktree_guard::git(
                worktree,
                &["merge-base", "--is-ancestor", &v.base_sha, base],
                &env,
            )
            .is_err()
        {
            return Err(format!(
                "verified on `{}`, which is not an ancestor of the base",
                v.base_sha
            ));
        }
        // `derivedFrom` matches as `invalidatedBy` does (`glob_match`), on the base's changes
        // since the probe (stale) and on the change itself (a head that edits what the guide was
        // derived from may not match it).
        let derived = |f: &String| guide.derived_from.iter().any(|g| glob_match(g, f));
        if !guide.derived_from.is_empty() {
            let since = git_paths(
                worktree,
                &["diff", "--name-only", "-z", v.base_sha.as_str(), base],
                &env,
            )
            .map_err(|e| format!("staleness could not be read: {e}"))?;
            if let Some(f) = since.iter().find(|f| derived(f)) {
                return Err(format!("stale: `{f}` changed since the guide was verified"));
            }
        }
        if let Some(f) = touched
            .iter()
            .find(|f| derived(f) || self.invalidated_by.iter().any(|g| glob_match(g, f)))
        {
            return Err(format!(
                "the change touches `{f}` (`invalidatedBy` / `derivedFrom`)"
            ));
        }
        Ok(())
    }

    /// The argv whose results the floor reads: `argv` with `results.how.argvAppend` ([`compose`]).
    pub(crate) fn for_results(&self, argv: &[String]) -> Vec<String> {
        compose(argv, &self.results.how.argv_append)
    }

    /// Do `alone` AND `deselect` both name the qualifier (`{q}`)? Otherwise an id is one test only
    /// when it is unique across every qualifier.
    pub(crate) fn q_scoped(&self) -> bool {
        let has_q = |v: &[String]| v.iter().any(|t| t.contains("{q}"));
        self.alone.as_ref().is_some_and(|a| has_q(&a.run))
            && self.deselect.as_ref().is_some_and(|d| has_q(&d.append))
    }

    /// The `alone` argv for `t`.
    pub(crate) fn alone_argv(&self, t: &TestId) -> Vec<String> {
        self.alone
            .as_ref()
            .map(|a| fill(&a.run, t))
            .unwrap_or_default()
    }

    /// `argv` with `ids` deselected: `deselect.append`'s tokens before its first per-id token
    /// (`{id}`, `{id:re}`, `{q}`) once, the tokens through the last one once per id, the rest once
    /// (`{ids:re|}`, every id regex-escaped and joined by `|`, fills a once-token), then
    /// [`compose`]d onto `argv`.
    pub(crate) fn deselected(&self, argv: &[String], ids: &[TestId]) -> Vec<String> {
        let tokens = self
            .deselect
            .as_ref()
            .map(|d| d.append.as_slice())
            .unwrap_or_default();
        let (first, last) = per_id_range(tokens);
        let mut extra = fill_all(&tokens[..first], ids);
        for t in ids {
            extra.extend(fill(&tokens[first..last], t));
        }
        extra.extend(fill_all(&tokens[last..], ids));
        compose(argv, &extra)
    }

    /// The `affected` argv for `files` (a token that is exactly `{files}` becomes one element per
    /// file; inside a token they are joined by spaces).
    /// `None` — the full check runs — with no file, or a file that would read as a flag (`-x`).
    pub(crate) fn affected_argv(&self, files: &[String]) -> Option<Vec<String>> {
        if files.is_empty() || files.iter().any(|f| f.starts_with('-')) {
            return None;
        }
        let a = self.affected.as_ref()?;
        let mut out = Vec::new();
        for t in &a.run {
            if t == "{files}" {
                out.extend(files.iter().cloned());
            } else {
                out.push(t.replace("{files}", &files.join(" ")));
            }
        }
        Some(out)
    }
}

/// `argv` with `extra` appended around its end of options (`--`): `extra`'s tokens before its own
/// `--` (all of them when it has none) are the runner's options and go just before `argv`'s `--`
/// (at the end when `argv` has none); its tokens after that `--` go last, behind ONE `--` (`argv`'s,
/// or the one `extra` brings). A second `--` is never emitted.
pub(crate) fn compose(argv: &[String], extra: &[String]) -> Vec<String> {
    let end_of_options = |v: &[String]| v.iter().position(|a| a == "--");
    let (opts, tail) = match end_of_options(extra) {
        Some(i) => (&extra[..i], Some(&extra[i + 1..])),
        None => (extra, None),
    };
    let mut out = argv.to_vec();
    let at = end_of_options(argv).unwrap_or(out.len());
    out.splice(at..at, opts.iter().cloned());
    if let Some(tail) = tail {
        if end_of_options(&out).is_none() {
            out.push("--".into());
        }
        out.extend(tail.iter().filter(|t| *t != "--").cloned());
    }
    out
}

/// The per-id tokens of `deselect.append`: from the first token with `{id}`, `{id:re}` or `{q}`
/// through the last one.
fn per_id_range(tokens: &[String]) -> (usize, usize) {
    let per = |t: &String| t.contains("{id}") || t.contains("{id:re}") || t.contains("{q}");
    let first = tokens.iter().position(per).unwrap_or(tokens.len());
    let last = tokens.iter().rposition(per).map_or(first, |i| i + 1);
    (first, last)
}

/// `{id}` and `{q}` verbatim, `{id:re}` regex-escaped (values fill argv elements; no shell).
/// One pass, so an id that holds a placeholder is never rewritten; `{ids:re|}` is this one id.
fn fill(argv: &[String], t: &TestId) -> Vec<String> {
    let slot = Regex::new(r"\{(?:id|id:re|ids:re\||q)\}").expect("static regex");
    argv.iter()
        .map(|a| {
            slot.replace_all(a, |c: &regex::Captures| match &c[0] {
                "{id:re}" | "{ids:re|}" => regex::escape(&t.id),
                "{id}" => t.id.clone(),
                _ => t.q.clone(),
            })
            .into_owned()
        })
        .collect()
}

/// `{ids:re|}`: every id, regex-escaped, joined by `|`, in one token.
fn fill_all(argv: &[String], ids: &[TestId]) -> Vec<String> {
    let all = || {
        ids.iter()
            .map(|t| regex::escape(&t.id))
            .collect::<Vec<_>>()
            .join("|")
    };
    argv.iter()
        .map(|a| {
            if a.contains("{ids:re|}") {
                a.replace("{ids:re|}", &all())
            } else {
                a.clone()
            }
        })
        .collect()
}

/// A probe result the floor may rely on: `pass`, or `pass (retried)` (it failed once and passed
/// its one retry).
pub(crate) fn passed(r: &str) -> bool {
    r == "pass" || r == "pass (retried)"
}

impl Compiled {
    /// The failing ids a run's text results name (kind text): `failed` line rules, and the items of
    /// `blocks` (a block opens at `start`; its contiguous `item` lines, blanks allowed, are its
    /// ids; any other line ends it; at `end` the last block since the previous end — or, with
    /// `all`, every one — is taken). Each id carries the `section` scope it appeared in.
    pub(crate) fn failures(&self, text: &str, keyed: bool) -> Vec<TestId> {
        let mut out: Vec<TestId> = Vec::new();
        let mut q = String::new();
        // Per block rule: the blocks since the last end, and whether the newest is still open.
        let mut open: Vec<(Vec<Vec<TestId>>, bool)> = vec![(Vec::new(), false); self.blocks.len()];
        // (`last-before-end`) The ONE block each qualifier ends with: an earlier footer-shaped
        // block (a test's captured output can print one) is replaced, never accumulated.
        let mut last: Vec<(String, Vec<TestId>)> = Vec::new();
        for l in text.lines().map(|l| l.trim_end_matches('\r')) {
            if let Some(s) = self.section.as_ref().and_then(|r| r.captures(l)) {
                q = s
                    .name("q")
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_default();
                // A new section drops every block the last one left uncommitted.
                for (blocks, is_open) in &mut open {
                    blocks.clear();
                    *is_open = false;
                }
            }
            for r in &self.failed {
                if let Some(id) = r.captures(l).and_then(|c| c.name("id")) {
                    out.push(TestId {
                        q: q.clone(),
                        id: id.as_str().to_string(),
                    });
                }
            }
            for ((start, item, end, all), (blocks, is_open)) in self.blocks.iter().zip(&mut open) {
                if end.is_match(l) {
                    let take: Vec<Vec<TestId>> = std::mem::take(blocks);
                    if *all {
                        out.extend(take.into_iter().flatten());
                    } else if let Some(b) = take.into_iter().rev().find(|b| !b.is_empty()) {
                        match last.iter_mut().find(|(k, _)| *k == q) {
                            Some((_, kept)) => *kept = b,
                            None => last.push((q.clone(), b)),
                        }
                    }
                    *is_open = false;
                } else if start.is_match(l) {
                    blocks.push(Vec::new());
                    *is_open = true;
                } else if *is_open {
                    match item.captures(l).and_then(|c| c.name("id")) {
                        Some(id) => blocks.last_mut().expect("open").push(TestId {
                            q: q.clone(),
                            id: id.as_str().to_string(),
                        }),
                        None if l.trim().is_empty() => {}
                        None => *is_open = false,
                    }
                }
            }
        }
        out.extend(last.into_iter().flat_map(|(_, b)| b));
        dedup(out, keyed)
    }

    /// How many tests a text run executed (`parser.count`, summed over its lines); `None` without
    /// a count rule or a line it matches.
    pub(crate) fn executed(&self, text: &str) -> Option<usize> {
        let r = self.count.as_ref()?;
        let mut total = None;
        for c in text
            .lines()
            .filter_map(|l| r.captures(l.trim_end_matches('\r')))
        {
            let n: usize = r
                .capture_names()
                .flatten()
                .filter_map(|g| c.name(g)?.as_str().parse::<usize>().ok())
                .sum();
            total = Some(total.unwrap_or(0) + n);
        }
        total
    }

    /// The ids a list run prints: `item` lines, each in the `section` scope it appeared in. Other
    /// lines are not the list's.
    pub(crate) fn list(&self, text: &str) -> Vec<TestId> {
        let mut out = Vec::new();
        let mut q = String::new();
        for l in text.lines().map(|l| l.trim_end_matches('\r')) {
            if let Some(s) = self.list_section.as_ref().and_then(|r| r.captures(l)) {
                q = s
                    .name("q")
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_default();
                continue;
            }
            if let Some(id) = self.list_item.captures(l).and_then(|c| c.name("id")) {
                out.push(TestId {
                    q: q.clone(),
                    id: id.as_str().to_string(),
                });
            }
        }
        out
    }

    /// Does the list carry a qualifier? When it does not, ids are compared without one.
    pub(crate) fn keyed(&self) -> bool {
        self.list_section.is_some()
    }
}

/// Unique, first-seen order; without a keyed list, the qualifier is dropped.
fn dedup(ids: Vec<TestId>, keyed: bool) -> Vec<TestId> {
    let mut seen = std::collections::HashSet::new();
    ids.into_iter()
        .map(|t| {
            if keyed {
                t
            } else {
                TestId {
                    q: String::new(),
                    ..t
                }
            }
        })
        .filter(|t| !t.id.is_empty() && seen.insert(t.clone()))
        .collect()
}

/// (junit) Every testcase that ran, one entry per `<testcase>` (one with a `skipped` child neither
/// passed nor failed), and the failing ones (a `failure` or `error` child): id = the `attrs`
/// values joined by `join` (a testcase whose attributes are all empty is dropped), q = the
/// nearest enclosing testsuite's `name`. Read with an XML parser (CDATA, `<system-out>`, nested
/// suites); `None` when the text is not XML or holds no testsuite.
pub(crate) fn junit(
    xml: &str,
    attrs: &[String],
    join: &str,
    keyed: bool,
) -> Option<(Vec<TestId>, Vec<TestId>)> {
    let (mut all, mut failed) = (Vec::new(), Vec::new());
    for (t, skipped, failing) in junit_cases(xml, attrs, join, keyed)? {
        if skipped {
            continue;
        }
        if failing {
            failed.push(t.clone());
        }
        all.push(t);
    }
    Some((all, dedup(failed, keyed)))
}

/// (junit) How many tests the run executed: every testcase with an id, run or skipped — the
/// count a list of every test matches.
pub(crate) fn junit_count(xml: &str, attrs: &[String]) -> Option<usize> {
    junit_cases(xml, attrs, "::", false).map(|c| c.len())
}

/// Every `<testcase>` with an id: (its id, skipped, failed). The one reading [`junit`] and
/// [`junit_count`] share.
fn junit_cases(
    xml: &str,
    attrs: &[String],
    join: &str,
    keyed: bool,
) -> Option<Vec<(TestId, bool, bool)>> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    if !doc.descendants().any(|n| n.has_tag_name("testsuite")) {
        return None;
    }
    let default_attrs = ["classname".to_string(), "name".to_string()];
    let attrs = if attrs.is_empty() {
        &default_attrs[..]
    } else {
        attrs
    };
    let mut out = Vec::new();
    for tc in doc.descendants().filter(|n| n.has_tag_name("testcase")) {
        let values: Vec<&str> = attrs
            .iter()
            .map(|a| tc.attribute(a.as_str()).unwrap_or_default())
            .collect();
        if values.iter().all(|v| v.is_empty()) {
            continue;
        }
        let child = |name: &str| tc.children().any(|c| c.has_tag_name(name));
        let q = tc
            .ancestors()
            .find(|s| s.has_tag_name("testsuite"))
            .and_then(|s| s.attribute("name"))
            .filter(|_| keyed)
            .unwrap_or_default();
        let t = TestId {
            q: q.to_string(),
            id: values.join(join),
        };
        out.push((t, child("skipped"), child("failure") || child("error")));
    }
    Some(out)
}

/// Repo-relative paths git prints NUL-separated (`-z`): each kept whole, never trimmed or
/// unquoted; a path that is not UTF-8 is an error (fail closed).
pub(crate) fn git_paths(
    cwd: &Path,
    args: &[&str],
    env: &[(&str, &Path)],
) -> Result<Vec<String>, String> {
    let out = crate::worktree_guard::git(cwd, args, env).map_err(|e| e.to_string())?;
    out.split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| {
            String::from_utf8(p.to_vec()).map_err(|_| "a changed path is not UTF-8".to_string())
        })
        .collect()
}

/// Does repo-relative `path` match `glob` (`**` any depth, `*` and `?` within a segment)?
pub(crate) fn glob_match(glob: &str, path: &str) -> bool {
    // `(?s)`: `**` crosses a newline in a name too.
    let mut p = String::from("(?s)^");
    let mut chars = glob.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                if chars.peek() == Some(&'/') {
                    chars.next();
                    p.push_str("(?:.*/)?");
                } else {
                    p.push_str(".*");
                }
            }
            '*' => p.push_str("[^/]*"),
            '?' => p.push_str("[^/]"),
            c => p.push_str(&regex::escape(&c.to_string())),
        }
    }
    p.push('$');
    Regex::new(&p).is_ok_and(|r| r.is_match(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn t(q: &str, id: &str) -> TestId {
        TestId {
            q: q.into(),
            id: id.into(),
        }
    }

    fn check(kind: &str) -> GuideCheck {
        serde_json::from_value(serde_json::json!({
            "name": "test", "run": ["runner", "go"], "failedExit": [3],
            "results": {"kind": kind},
            "parser": {"section": "^== (?P<q>\\S+)$", "failed": ["^BAD (?P<id>\\S+)$"],
                       "blocks": [{"start": "^summary:$", "item": "^  (?P<id>\\S+)$", "end": "^done: red"}]},
            "list": {"run": ["runner", "list"], "item": "^(?P<id>\\S+)$", "section": "^== (?P<q>\\S+)$"},
            "alone": {"run": ["runner", "one", "{id}"]},
            "deselect": {"append": ["--", "--not={id}", "--exact"]},
        }))
        .unwrap()
    }

    /// A guide is checked on load: an empty `run`, an unknown `stage`, or a test check with no
    /// `failedExit` does not parse; `{q}` inside an id is never rewritten.
    #[test]
    fn an_invalid_guide_does_not_parse_and_fill_is_one_pass() {
        let guide = |check: serde_json::Value| {
            parse(&serde_json::json!({"version": 1, "checks": [check]}).to_string())
        };
        assert!(guide(serde_json::json!({"name": "g", "run": ["x"]})).is_ok());
        assert!(guide(serde_json::json!({"name": "g", "run": []})).is_err());
        assert!(guide(serde_json::json!({"name": "g", "run": ["x"], "stage": ["ci"]})).is_err());
        let mut c = serde_json::to_value(check("text")).unwrap();
        c["failedExit"] = serde_json::json!([]);
        assert!(guide(c.clone()).is_err());
        // (round 17) A failure exits nonzero: `0` is refused, as is a `cwd` outside the repo,
        // `{ids:re|}` among the per-id tokens, and `{ids:re|}` beside `{q}`.
        for (k, v) in [
            ("failedExit", serde_json::json!([0, 3])),
            ("cwd", serde_json::json!("../x")),
            ("cwd", serde_json::json!("/abs")),
            ("cwd", serde_json::json!("a//b")),
            ("cwd", serde_json::json!("a/./b")),
            ("cwd", serde_json::json!("C:x")),
            (
                "deselect",
                serde_json::json!({"append": ["--suite={q}", "--skip={id}"]}),
            ),
            (
                "deselect",
                serde_json::json!({"append": ["--not={id}", "--all={ids:re|}", "--raw={id}"]}),
            ),
            (
                "deselect",
                serde_json::json!({"append": ["--suite={q}", "--p=^(?!(?:{ids:re|})$)"]}),
            ),
        ] {
            let mut bad = serde_json::to_value(check("text")).unwrap();
            bad[k] = v.clone();
            assert!(guide(bad).is_err(), "{k}: {v}");
        }
        let mut ok = serde_json::to_value(check("text")).unwrap();
        ok["cwd"] = "crates/x".into();
        assert!(guide(ok).is_ok());
        assert_eq!(
            fill(&s(&["--{id}@{q}"]), &t("Q", "a{q}b")),
            s(&["--a{q}b@Q"])
        );
    }

    /// (round 17) The committed seed guide parses and passes the load checks.
    #[test]
    fn the_committed_seed_guide_parses() {
        let g = parse(include_str!("../.wicked/build-guide.json")).expect("the seed guide parses");
        assert!(g
            .checks
            .iter()
            .any(|c| c.name == "test" && c.list.is_some()));
    }

    /// A new section drops an uncommitted block; an empty block never hides the one before it;
    /// a footer-shaped block an earlier line printed is REPLACED by the qualifier's last one.
    #[test]
    fn a_section_change_drops_open_blocks_and_an_empty_block_hides_nothing() {
        let c = check("text").compile().unwrap();
        let out = "== bin1\nsummary:\n  stale\n== bin2\nsummary:\n  real\nsummary:\ndone: red\n";
        assert_eq!(c.failures(out, true), vec![t("bin2", "real")]);
        let forged = "== bin1\nsummary:\n  fine\ndone: red\nsummary:\n  old\ndone: red\n";
        assert_eq!(c.failures(forged, true), vec![t("bin1", "old")]);
    }

    /// `parser.count` sums every named group of each line it matches; JUnit counts every case
    /// with an id, run or skipped.
    #[test]
    fn the_executed_count_reads_count_lines_and_junit_cases() {
        let mut g = check("text");
        g.parser.as_mut().unwrap().count =
            Some(r"^result: (?P<n>\d+) ok, (?P<f>\d+) bad, (?P<i>\d+) off$".into());
        let c = g.compile().unwrap();
        assert_eq!(
            c.executed("result: 2 ok, 1 bad, 1 off\nx\nresult: 3 ok, 0 bad, 0 off\n"),
            Some(7)
        );
        assert_eq!(
            check("text")
                .compile()
                .unwrap()
                .executed("result: 1 ok, 0 bad, 0 off"),
            None
        );
        let xml = r#"<testsuite name="s"><testcase name="a"/><testcase name="b"><skipped/></testcase><testcase/></testsuite>"#;
        assert_eq!(junit_count(xml, &s(&["name"])), Some(2));
    }

    /// `argvAppend` and `deselect.append` compose around an existing `--`: options before it,
    /// test-binary arguments after it, and never a second `--`.
    #[test]
    fn argv_composition_keeps_one_end_of_options() {
        let v = |x: &[&str]| s(x);
        assert_eq!(
            compose(&v(&["r", "t"]), &v(&["--o"])),
            v(&["r", "t", "--o"])
        );
        assert_eq!(
            compose(&v(&["r", "t", "--", "--list"]), &v(&["--o", "--", "--x"])),
            v(&["r", "t", "--o", "--", "--list", "--x"])
        );
        assert_eq!(
            compose(&v(&["r", "t"]), &v(&["--", "--x", "--", "--y"])),
            v(&["r", "t", "--", "--x", "--y"])
        );
        let mut g = check("text");
        g.results.how.argv_append = v(&["--report", "--", "--show"]);
        let both = g.for_results(&g.deselected(&v(&["r", "go"]), &[t("", "a")]));
        assert_eq!(
            both,
            v(&["r", "go", "--report", "--", "--not=a", "--exact", "--show"])
        );
        assert_eq!(both.iter().filter(|a| *a == "--").count(), 1);
    }

    /// The text parser: `failed` rules and the LAST block before each end, each id in its
    /// section; an item-shaped line after a non-item line inside a block is not the block's.
    #[test]
    fn the_text_parser_reads_line_rules_and_the_last_block_in_its_section() {
        let c = check("text").compile().unwrap();
        let out =
            "== bin1\nBAD a\nsummary:\n\n--- captured\n  ghost\nsummary:\n  b\n  c\n\ndone: red\n\
                   == bin2\nsummary:\n  d\ndone: red\n  e\n";
        assert_eq!(
            c.failures(out, true),
            vec![
                t("bin1", "a"),
                t("bin1", "b"),
                t("bin1", "c"),
                t("bin2", "d")
            ]
        );
        assert_eq!(
            c.failures(out, false)[0],
            t("", "a"),
            "unkeyed: no qualifier"
        );
        let mut all = check("text");
        all.parser.as_mut().unwrap().blocks[0].use_ = "all".into();
        let out2 = "summary:\n  x\nnoise\nsummary:\n  y\ndone: red\n";
        assert_eq!(
            all.compile().unwrap().failures(out2, false),
            vec![t("", "x"), t("", "y")]
        );
        assert_eq!(
            c.list("== bin1\na\nb\n== bin2\na\n"),
            vec![t("bin1", "a"), t("bin1", "b"), t("bin2", "a")]
        );
        assert!(
            check("tap").compile().is_err(),
            "reserved kinds are not read"
        );
    }

    /// junit: every testcase and the failing ones; id = the configured attributes, q = suite.
    #[test]
    fn the_junit_reader_takes_testcases_and_failures() {
        let xml = r#"<?xml version="1.0"?><testsuites><testsuite name="s1">
            <testcase classname="m.a" name="ok_one" file="m/a.x"/>
            <testcase classname="m.a" name="bad &amp; worse" file="m/a.x"><failure message="x"/></testcase>
            <testcase classname="m.b" name="err" file="m/b.x"><error/></testcase>
            <testcase classname="m.b" name="left_out" file="m/b.x"><skipped/></testcase>
            </testsuite></testsuites>"#;
        let (all, failed) = junit(xml, &[], "::", true).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(
            failed,
            vec![t("s1", "m.a::bad & worse"), t("s1", "m.b::err")]
        );
        let (_, by_file) = junit(xml, &s(&["file", "name"]), "::", false).unwrap();
        assert_eq!(by_file[1], t("", "m/b.x::err"));
        assert!(junit("no xml here", &[], "::", true).is_none());
        // Per `<testcase>`: a skipped twin does not erase the one that ran; two that ran are two.
        let twins = |second: &str| {
            format!(r#"<testsuite name="s"><testcase classname="c" name="x"/>{second}</testsuite>"#)
        };
        let skipped = twins(r#"<testcase classname="c" name="x"><skipped/></testcase>"#);
        assert_eq!(junit(&skipped, &[], "::", false).unwrap().0.len(), 1);
        let ran = twins(r#"<testcase classname="c" name="x"></testcase>"#);
        assert_eq!(junit(&ran, &[], "::", false).unwrap().0.len(), 2);
        // A real XML reader: tags inside CDATA or `<system-out>` are text, nested suites qualify
        // their own cases, entities decode once, and a case with no id attributes is dropped.
        let tricky = r#"<testsuites><testsuite name="outer"><testsuite name="inner">
            <testcase classname="c" name="a &amp;quot;q&amp;quot;"><system-out><![CDATA[<failure/><skipped/>]]></system-out></testcase>
            </testsuite><testcase classname="c" name="b"><failure>&lt;skipped/&gt;</failure></testcase>
            <testcase/></testsuite></testsuites>"#;
        let (all, failed) = junit(tricky, &[], "::", true).unwrap();
        assert_eq!(
            all,
            vec![t("inner", "c::a &quot;q&quot;"), t("outer", "c::b")]
        );
        assert_eq!(failed, vec![t("outer", "c::b")]);
    }

    /// Deselect: tokens before the first `{id}` once (a `--` already in the argv is not
    /// repeated), the `{id}` tokens per id, the rest once; alone and affected fill their templates.
    #[test]
    fn templates_fill_per_id_and_once() {
        let g = check("text");
        assert_eq!(
            g.deselected(&s(&["runner", "go"]), &[t("q", "a"), t("q", "b")]),
            s(&["runner", "go", "--", "--not=a", "--not=b", "--exact"])
        );
        assert_eq!(
            g.deselected(&s(&["runner", "list", "--", "x"]), &[t("q", "a")]),
            s(&["runner", "list", "--", "x", "--not=a", "--exact"])
        );
        assert_eq!(g.alone_argv(&t("q", "a")), s(&["runner", "one", "a"]));
        // `{id:re}` repeats per id, regex-escaped; `{ids:re|}` is one token over every id; plain
        // `{id}` stays verbatim.
        let mut r = g.clone();
        r.deselect = Some(Deselect {
            append: s(&[
                "--pattern=^(?!(?:{ids:re|})$)",
                "--not={id:re}",
                "--raw={id}",
            ]),
        });
        assert_eq!(
            r.deselected(&s(&["runner", "go"]), &[t("", "a.b"), t("", "c(d)")]),
            s(&[
                "runner",
                "go",
                r"--pattern=^(?!(?:a\.b|c\(d\))$)",
                r"--not=a\.b",
                "--raw=a.b",
                r"--not=c\(d\)",
                "--raw=c(d)",
            ])
        );
        r.alone = Some(RunPiece {
            run: s(&["runner", "--only=^{id:re}$", "--any={ids:re|}"]),
        });
        assert_eq!(
            r.alone_argv(&t("", "x+y")),
            s(&["runner", r"--only=^x\+y$", r"--any=x\+y"])
        );
        let mut a = g.clone();
        a.affected = Some(RunPiece {
            run: s(&["runner", "go", "{files}"]),
        });
        assert_eq!(
            a.affected_argv(&s(&["x.y", "z.y"])).unwrap(),
            s(&["runner", "go", "x.y", "z.y"])
        );
    }

    #[test]
    fn globs_match_by_segment_and_any_depth() {
        assert!(glob_match(".cfg/**", ".cfg/a/b.toml"));
        assert!(glob_match("**/.cfg/**", "x/y/.cfg/c"));
        assert!(glob_match("**/*.lock", "a/b/c.lock") && glob_match("**/*.lock", "c.lock"));
        assert!(!glob_match("*.lock", "a/c.lock"));
        assert!(glob_match("tool-?", "tool-1") && !glob_match("tool-?", "tool-12"));
    }

    /// The guide is the allowlist: a floor check matches only on its exact argv and cwd; a check
    /// runs at the stages it names (both when it names none).
    #[test]
    fn a_check_matches_only_its_exact_argv_and_cwd() {
        let mut verify_only = check("text");
        verify_only.name = "slow".into();
        verify_only.stage = s(&["verify"]);
        let g = BuildGuide {
            version: 1,
            derived_from: vec![],
            checks: vec![check("text"), verify_only],
        };
        let names = |stage| {
            g.checks_at(stage)
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(names("creator"), ["test"]);
        assert_eq!(names("verify"), ["test", "slow"]);
        assert!(passed("pass") && passed("pass (retried)") && !passed("fail: x (retried)"));
        assert!(g.check_for(&s(&["runner", "go"]), "").is_some());
        assert!(g.check_for(&s(&["runner", "go"]), ".").is_some());
        assert!(g.check_for(&s(&["runner", "go", "x"]), "").is_none());
        assert!(g.check_for(&s(&["runner", "go"]), "sub").is_none());
    }

    /// The schema (`schemas/build-guide.v1.json`) declares every field these types write.
    #[test]
    fn the_schema_declares_every_field_the_types_write() {
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../schemas/build-guide.v1.json")).unwrap();
        let mut c = check("text");
        c.affected = Some(RunPiece { run: s(&["x"]) });
        c.invalidated_by = s(&["a/**"]);
        c.results.how = How {
            argv_append: s(&["--r"]),
            env: [("K".to_string(), "v".to_string())].into(),
            path: Some("r.xml".into()),
            merge: true,
        };
        c.results.id = s(&["file", "name"]);
        c.results.id_join = Some(" > ".into());
        c.probes.mutation = Some(Mutation {
            file: "m".into(),
            content: "c".into(),
            expect_id: "i".into(),
            expect_q: Some("q".into()),
        });
        c.verified = Some(Verified::default());
        let wrote = serde_json::to_value(&c).unwrap();
        let props = |v: &serde_json::Value| -> Vec<String> {
            v["properties"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect()
        };
        let check_props = props(&schema["$defs"]["check"]);
        for k in wrote.as_object().unwrap().keys() {
            assert!(
                check_props.contains(k),
                "`{k}` is written but not in the schema"
            );
        }
        let results = &schema["$defs"]["check"]["properties"]["results"];
        for k in wrote["results"].as_object().unwrap().keys() {
            assert!(props(results).contains(k), "results.{k}");
        }
        for k in wrote["results"]["how"].as_object().unwrap().keys() {
            assert!(
                props(&results["properties"]["how"]).contains(k),
                "results.how.{k}"
            );
        }
    }
}
