//! How much review a unit's change summons (S4 of #590).
//!
//! A per-phase council costs six CLI subprocesses whether or not anything interesting happened.
//! Teaming (#590) replaces it with monitors whose number and depth scale with the change. This
//! module is that scaling, and nothing else: a PURE function from a unit's change signals to a
//! [`ReviewPlan`], plus [`signals_from_diff`], which derives those signals from a unified diff so
//! a caller never has to know the heuristics. Nothing here dispatches a monitor; S2 (monitors) and
//! S5 (teamed mode) call it.
//!
//! Every number and word list lives in [`THRESHOLDS`], so tuning the policy is a one-table edit.

use std::collections::BTreeSet;

/// What a unit changed. Plain data, so a caller that already has its own counts can skip
/// [`signals_from_diff`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ChangeSignals {
    pub lines_added: u32,
    pub lines_removed: u32,
    pub docs_files: u32,
    pub code_files: u32,
    pub test_files: u32,
    pub config_files: u32,
    /// Distinct subsystems among the non-docs files.
    pub subsystems: u32,
    /// A non-docs file sits in a critical subsystem ([`Thresholds::critical_path_markers`]).
    pub critical: bool,
    /// The change touches a destructive path: a delete, erase, force, migration, and so on.
    pub destructive: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Depth {
    None,
    Standard,
    Deep,
}

/// The review a change gets: live monitors, how deep they read, and whether an independent
/// reviewer reads the finished change afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReviewPlan {
    pub monitors: u8,
    pub depth: Depth,
    pub post_hoc_reviewer: bool,
}

/// The tunable policy. One table, so an operator changes the policy here and nowhere else.
pub(crate) struct Thresholds {
    /// Monitors for any behavioural (non-docs) change.
    pub base_monitors: u8,
    /// Ceiling: the most review any change gets.
    pub max_monitors: u8,
    /// Changed lines (added + removed) at which a change counts as large.
    pub large_lines: u32,
    /// Non-docs files at which a change counts as large.
    pub large_files: u32,
    /// Distinct subsystems at which a change counts as cross-subsystem.
    pub cross_subsystems: u32,
    /// Escalations (destructive, critical, large, cross-subsystem) that summon a post-hoc reviewer.
    pub post_hoc_min_escalations: u32,
    /// Escalations that make monitors read deep. A destructive path is always deep.
    pub deep_min_escalations: u32,
    /// File extensions that are prose.
    pub docs_exts: &'static [&'static str],
    /// File extensions that are configuration.
    pub config_exts: &'static [&'static str],
    /// Path-token prefixes of critical subsystems.
    pub critical_path_markers: &'static [&'static str],
    /// Substrings (lowercased) of a changed line in a non-docs file that mark a destructive path.
    pub destructive_line_markers: &'static [&'static str],
    /// Path-token prefixes that make a whole file destructive.
    pub destructive_path_markers: &'static [&'static str],
}

/// The policy's values, and why.
///
/// - **Docs-only gets nothing** (`base_monitors` applies only to non-docs files): across this
///   program 6/6 docs PRs landed clean first time. Destructive words in prose (a README quoting
///   `rm -rf`) are not a destructive path, so line markers are read from non-docs files only.
/// - **Any behavioural change gets one monitor** (`base_monitors = 1`): 13/13 behavioural PRs came
///   back with findings, so no behavioural change goes unwatched.
/// - **A destructive path is an automatic second pair of eyes plus a post-hoc reviewer, however
///   small the diff**: the issue's HIGH was a one-handler defect that showed "1 memory will be
///   erased" before a destructive action, and it survived the creator, the evaluator, a human gate
///   and a PR. A destructive path counts as an escalation and always reads deep.
/// - **Post-hoc reviewer at the first escalation** (`post_hoc_min_escalations = 1`): the
///   independent reviewer found 5 defects the in-run review missed, so it stays where impact
///   outweighs cost, and a small, local, non-critical, non-destructive change gets the live monitor
///   instead. The 13/13 figure would also support `0` here (a post-hoc reviewer on every
///   behavioural change); that is the knob to turn if small changes keep leaking findings.
/// - **Size** (`large_lines = 400`, `large_files = 15`): the issue gives no size figure. 400 changed
///   lines is the commonly cited point past which a single reviewer's defect yield falls off; treat
///   it as a starting value to tune against run evidence.
/// - **Cross-subsystem at 3** distinct subsystems: two is an ordinary caller-and-callee change;
///   three is where "correct locally, wrong for the system" (the issue's authority-model examples)
///   becomes the likely failure.
/// - **Ceiling 3 monitors**: a monitor that flags everything is noise (issue risk 2), and more
///   concurrent seats is what made councils starve each other.
pub(crate) const THRESHOLDS: Thresholds = Thresholds {
    base_monitors: 1,
    max_monitors: 3,
    large_lines: 400,
    large_files: 15,
    cross_subsystems: 3,
    post_hoc_min_escalations: 1,
    deep_min_escalations: 2,
    docs_exts: &["md", "mdx", "markdown", "rst", "adoc", "txt"],
    config_exts: &["toml", "json", "yaml", "yml", "lock", "ini", "cfg", "env"],
    critical_path_markers: &[
        "memory",
        "gate",
        "governance",
        "fence",
        "deliver",
        "migration",
        "credential",
        "secret",
        "state_home",
        "path_policy",
        "write_posture",
    ],
    destructive_line_markers: &[
        "remove_dir_all",
        "remove_file",
        "rm -rf",
        "rm -r ",
        "rmsync",
        "rimraf",
        "unlink",
        "erase",
        "purge",
        "wipe",
        "--force",
        "push -f",
        "reset --hard",
        "drop table",
        "drop column",
        "truncate table",
        "delete from",
    ],
    destructive_path_markers: &["migration"],
};

/// How much review a change gets. The one policy entry point.
pub(crate) fn review_plan(s: &ChangeSignals) -> ReviewPlan {
    let t = &THRESHOLDS;
    let behavioural = s.code_files + s.test_files + s.config_files;
    if behavioural == 0 && !s.destructive {
        return ReviewPlan {
            monitors: 0,
            depth: Depth::None,
            post_hoc_reviewer: false,
        };
    }
    let large = s.lines_added + s.lines_removed >= t.large_lines || behavioural >= t.large_files;
    let cross = s.subsystems >= t.cross_subsystems;
    let escalations = [s.destructive, s.critical, large, cross]
        .iter()
        .filter(|e| **e)
        .count() as u32;
    let deep = s.destructive || escalations >= t.deep_min_escalations;
    ReviewPlan {
        monitors: (u32::from(t.base_monitors) + escalations).min(u32::from(t.max_monitors)) as u8,
        depth: if deep { Depth::Deep } else { Depth::Standard },
        post_hoc_reviewer: escalations >= t.post_hoc_min_escalations,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Docs,
    Test,
    Config,
    Code,
}

/// Derive [`ChangeSignals`] from a unified diff (`git diff` output).
///
/// Fails closed: a file is behavioural if EITHER side of its header is (a rename from
/// `src/memory.rs` to `docs/memory.md` moves code out of a critical subsystem), and a hunk with no
/// file header counts as code.
pub(crate) fn signals_from_diff(diff: &str) -> ChangeSignals {
    let t = &THRESHOLDS;
    let mut s = ChangeSignals::default();
    let mut subsystems = BTreeSet::new();
    let mut kind = None;
    let mut in_hunk = false;
    for line in diff.lines() {
        let header = line.strip_prefix("diff --git ");
        if header.is_some() || (kind.is_none() && line.starts_with("@@")) {
            let rest = header.unwrap_or("a/ b/");
            let (a, b) = rest.split_once(" b/").unwrap_or((rest, rest));
            let sides = [a.strip_prefix("a/").unwrap_or(a), b];
            let code: Vec<&str> = sides
                .into_iter()
                .filter(|p| !p.is_empty() && classify(p) != Kind::Docs)
                .collect();
            let k = match code.last() {
                Some(p) => classify(p),
                None if header.is_none() => Kind::Code,
                None => Kind::Docs,
            };
            match k {
                Kind::Docs => s.docs_files += 1,
                Kind::Test => s.test_files += 1,
                Kind::Config => s.config_files += 1,
                Kind::Code => s.code_files += 1,
            }
            subsystems.extend(code.iter().map(|p| subsystem(p)));
            if k != Kind::Docs {
                for p in sides {
                    s.critical |= has_token(p, t.critical_path_markers);
                    s.destructive |= has_token(p, t.destructive_path_markers);
                }
            }
            kind = Some(k);
            in_hunk = false;
        }
        let behavioural = kind.is_some_and(|k| k != Kind::Docs);
        if line.starts_with("@@") {
            in_hunk = true;
        } else if !in_hunk {
            s.destructive |= behavioural && line.starts_with("deleted file mode");
        } else if let Some(text) = line.strip_prefix('+').or_else(|| line.strip_prefix('-')) {
            if line.starts_with('+') {
                s.lines_added += 1;
            } else {
                s.lines_removed += 1;
            }
            if behavioural {
                let text = text.to_ascii_lowercase();
                s.destructive |= t.destructive_line_markers.iter().any(|m| text.contains(m));
            }
        }
    }
    s.subsystems = subsystems.len() as u32;
    s
}

fn classify(path: &str) -> Kind {
    let t = &THRESHOLDS;
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit_once('.').map_or("", |(_, e)| e);
    if t.docs_exts.contains(&ext) {
        Kind::Docs
    } else if path
        .split('/')
        .any(|c| matches!(c, "tests" | "test" | "__tests__"))
        || name.starts_with("test_")
        || [".test.", ".spec.", "_test."]
            .iter()
            .any(|m| name.contains(m))
    {
        Kind::Test
    } else if t.config_exts.contains(&ext) || path.split('/').any(|c| c.starts_with('.')) {
        Kind::Config
    } else {
        Kind::Code
    }
}

/// `crates/<name>` and `packages/<name>` are subsystems; elsewhere the first directory plus the
/// next component's stem (`src/distribute.rs` is `src/distribute`); a root file is `.`.
fn subsystem(path: &str) -> String {
    let c: Vec<&str> = path.split('/').collect();
    match c.as_slice() {
        [root @ ("crates" | "packages"), name, _, ..] => format!("{root}/{name}"),
        [first, next, ..] => format!("{first}/{}", next.split('.').next().unwrap_or(next)),
        _ => ".".to_string(),
    }
}

/// A path token (split on `/ _ - .`) starts with one of `markers`.
fn has_token(path: &str, markers: &[&str]) -> bool {
    let path = path.to_ascii_lowercase();
    path.split(['/', '_', '-', '.'])
        .any(|tok| markers.iter().any(|m| tok.starts_with(m)))
        || markers.iter().any(|m| m.contains('_') && path.contains(m))
}

#[cfg(test)]
mod tests;
