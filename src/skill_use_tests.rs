//! Skill usage: one test per CLI signal, each through the CLI's own adapter, so a detector that is
//! not wired (or not called) fails it; plus the guards (worktree paths, listings, globs, comments,
//! non-reading programs, missing scripts, failed calls, browse, dedupe across relaunches, the frame
//! rate limit and clip, privacy) and the directed / handed halves.

// The POSIX shell cases run on Unix only; their helpers are unused on Windows.
#![cfg_attr(windows, allow(dead_code, unused_imports))]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use super::*;
use crate::event::CoreEvent;
use crate::execute_wrapped::{
    AgyStreamJson, ClaudeStreamJson, CodexJsonl, OpencodeJson, OutputAdapter, PiJson,
};
use crate::skills_snapshot::test_support::{gen_dir, load, scratch, snapshot_root_with};
use crate::skills_snapshot::{SkillsDelivery, SkillsLever, SkillsSnapshot, WorkerCli};

type Seen = Arc<Mutex<Vec<CoreEvent>>>;

const SCRIPTS: [&str; 6] = [
    "domain/extract_loop.py",
    "qe/prove.py",
    "qe/vault_gate.py",
    "learn/check.py",
    "wicked-garden",
    "wicked-garden.mjs",
];

/// A published snapshot with a nested skill, a non-portable one, four browse targets and the
/// scripts the tests run.
fn fixture() -> (SkillsSnapshot, PathBuf) {
    let base = scratch("skilluse");
    let gen = gen_dir(&base, "000042");
    for s in SCRIPTS {
        let p = s.split('/').fold(gen.join("scripts"), |p, c| p.join(c));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
    }
    let root = snapshot_root_with(
        &gen,
        "000042",
        &[
            ("domain", "wicked-garden-domain", true, &[]),
            (
                "domain-extractor",
                "wicked-garden-domain-extractor",
                true,
                &[],
            ),
            ("qe", "wicked-garden-qe", true, &[]),
            ("qe/oracle", "wicked-garden-qe-oracle", true, &[]),
            ("claude-only", "wicked-garden-claude-only", false, &[]),
            ("a", "wicked-garden-a", true, &[]),
            ("b", "wicked-garden-b", true, &[]),
            ("c", "wicked-garden-c", true, &[]),
            ("d", "wicked-garden-d", true, &[]),
        ],
    );
    // Support files of `domain`: an extensionless one and a directory with a dot.
    let refs = root.join("skills").join("domain").join("references");
    std::fs::create_dir_all(&refs).unwrap();
    std::fs::write(refs.join("README"), "refs\n").unwrap();
    std::fs::create_dir_all(root.join("skills").join("domain").join("refs.d")).unwrap();
    (load(&root), base)
}

/// A session id no other test uses: dedupe is per unit attempt, process-wide.
fn session() -> String {
    static N: AtomicU32 = AtomicU32::new(0);
    format!("su-test-{}", N.fetch_add(1, Ordering::Relaxed))
}

fn cli(key: &str, lever: SkillsLever) -> WorkerCli {
    WorkerCli::Other {
        key: key.to_string(),
        lever,
    }
}

/// A sink for `cli` on `delivery`, recording what it emits.
fn sink_for(
    snap: &SkillsSnapshot,
    delivery: &SkillsDelivery,
    codex_home: Option<&Path>,
    cli: &str,
    session: &str,
) -> (SkillUseSink, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let tx = Arc::clone(&seen);
    let paths = Arc::new(SkillPaths::new(snap, delivery, codex_home));
    let sink = SkillUseSink::new(
        session,
        3,
        0,
        cli,
        snap,
        paths,
        Arc::new(move |ev| tx.lock().unwrap().push(ev)),
    );
    (sink, seen)
}

/// `(signal, skill or script)` of every event, in order.
fn signals(seen: &Seen) -> Vec<(String, String)> {
    seen.lock()
        .unwrap()
        .iter()
        .map(|e| match e {
            CoreEvent::SkillUsed {
                signal,
                skill,
                script,
                ..
            } => (
                signal.clone(),
                skill.clone().or_else(|| script.clone()).unwrap_or_default(),
            ),
            other => panic!("not a skillUsed event: {other:?}"),
        })
        .collect()
}

fn pair(signal: &str, what: &str) -> (String, String) {
    (signal.to_string(), what.to_string())
}

fn skill_file(snap: &SkillsSnapshot, dir: &str) -> String {
    dir.split('/')
        .fold(snap.root.join("skills"), |p, c| p.join(c))
        .join("SKILL.md")
        .to_string_lossy()
        .into_owned()
}

fn claude(snap: &SkillsSnapshot) -> (ClaudeStreamJson, Seen) {
    let d = snap.delivery_with(&WorkerCli::Claude, false);
    let (s, seen) = sink_for(snap, &d, None, "claude", &session());
    (ClaudeStreamJson::default().with_skill_use(Some(s)), seen)
}

/// A claude tool call and its result.
fn claude_call(a: &mut ClaudeStreamJson, id: &str, name: &str, input: serde_json::Value, ok: bool) {
    a.on_line(
        &serde_json::json!({"type": "assistant", "message": {"id": "m1", "content": [
            {"type": "tool_use", "id": id, "name": name, "input": input}]}})
        .to_string(),
    );
    a.on_line(
        &serde_json::json!({"type": "user", "message": {"content": [
            {"type": "tool_result", "tool_use_id": id, "is_error": !ok, "content": "x"}]}})
        .to_string(),
    );
}

fn bash(a: &mut ClaudeStreamJson, id: &str, cmd: &str) {
    claude_call(a, id, "Bash", serde_json::json!({ "command": cmd }), true);
}

// ── claude ─────────────────────────────────────────────────────────────────────────────────────

#[test]
fn claude_skill_tool_is_opened_by_its_delivered_plugin_identity_only() {
    let (snap, _b) = fixture();
    let (mut a, seen) = claude(&snap);
    claude_call(
        &mut a,
        "t1",
        "Skill",
        serde_json::json!({"skill": "wicked-garden:domain-extractor"}),
        true,
    );
    assert_eq!(
        signals(&seen),
        vec![pair("opened", "wicked-garden-domain-extractor")]
    );
    let json = seen.lock().unwrap()[0].to_json();
    assert_eq!(json["type"], "skillUsed");
    assert_eq!(json["how"], "skill_tool");
    assert_eq!(json["cli"], "claude");
    assert_eq!(json["gen"], "000042");
    assert_eq!(json["source"], "published");
    // Another plugin's skill, a bare name, a nested dir (no Claude identity): none is ours.
    for arg in [
        "superpowers:brainstorm",
        "wicked-garden-qe-oracle",
        "wicked-garden:qe/oracle",
    ] {
        claude_call(
            &mut a,
            arg,
            "Skill",
            serde_json::json!({ "skill": arg }),
            true,
        );
    }
    assert_eq!(signals(&seen).len(), 1);
}

#[test]
fn claude_read_of_a_nested_skill_file_opens_the_deepest_owner_once_it_succeeded() {
    let (snap, _b) = fixture();
    let (mut a, seen) = claude(&snap);
    // A refused read loaded nothing.
    claude_call(
        &mut a,
        "t0",
        "Read",
        serde_json::json!({"file_path": skill_file(&snap, "qe")}),
        false,
    );
    claude_call(
        &mut a,
        "t1",
        "Read",
        serde_json::json!({"file_path": skill_file(&snap, "qe/oracle")}),
        true,
    );
    assert_eq!(
        signals(&seen),
        vec![pair("opened", "wicked-garden-qe-oracle")]
    );
    assert_eq!(seen.lock().unwrap()[0].to_json()["how"], "read");
}

#[test]
fn claude_bash_running_a_garden_script_is_ran_with_no_arguments_kept() {
    let (snap, _b) = fixture();
    let (mut a, seen) = claude(&snap);
    bash(
        &mut a,
        "t1",
        "cd /work/tree && wicked-garden run scripts/domain/extract_loop.py --check-rules /home/someone/secret/rules.json",
    );
    assert_eq!(
        signals(&seen),
        vec![pair("ran", "scripts/domain/extract_loop.py")]
    );
    let json = seen.lock().unwrap()[0].to_json().to_string();
    assert!(!json.contains("secret") && !json.contains("/home/") && !json.contains("--check"));
    assert!(json.contains("\"skill\":null"));
}

// A POSIX shell line: a Windows host path in one is itself a run of escapes, so these run on Unix.
#[cfg(unix)]
#[test]
fn a_nested_shell_is_read_inside() {
    let (snap, _b) = fixture();
    let (mut a, seen) = claude(&snap);
    bash(
        &mut a,
        "t1",
        &format!("bash -lc 'cat {}'", skill_file(&snap, "domain")),
    );
    assert_eq!(signals(&seen), vec![pair("opened", "wicked-garden-domain")]);
}

// ── codex ──────────────────────────────────────────────────────────────────────────────────────

/// A codex launch: its home populated by the engine's own lever, as the runner does.
fn codex(snap: &SkillsSnapshot, home: &Path) -> (CodexJsonl, Seen) {
    let d = snap.delivery_with(&cli("codex", SkillsLever::CodexSkillsDir), false);
    crate::skills_snapshot::populate_seat_home(&d, Some(home)).unwrap();
    let (s, seen) = sink_for(snap, &d, Some(home), "codex", &session());
    (CodexJsonl::default().with_skill_use(Some(s)), seen)
}

fn codex_cmd(a: &mut CodexJsonl, id: &str, cmd: &str, exit: i64) {
    a.on_line(
        &serde_json::json!({"type": "item.started", "item": {"id": id, "type": "command_execution",
            "command": cmd, "status": "in_progress"}})
        .to_string(),
    );
    a.on_line(
        &serde_json::json!({"type": "item.completed", "item": {"id": id, "type": "command_execution",
            "command": cmd, "exit_code": exit, "status": if exit == 0 { "completed" } else { "failed" }}})
        .to_string(),
    );
}

// A POSIX shell line: a Windows host path in one is itself a run of escapes, so these run on Unix.
#[cfg(unix)]
#[test]
fn codex_shell_read_of_its_home_skill_file_is_opened_and_a_listing_is_not() {
    let (snap, base) = fixture();
    let home = base.join("codex-home");
    let (mut a, seen) = codex(&snap, &home);
    let file = home.join("skills/wicked-garden-domain/SKILL.md");
    codex_cmd(
        &mut a,
        "i1",
        &format!(
            "/bin/bash -lc \"sed -n '1,200p' {}\"",
            file.to_string_lossy()
        ),
        0,
    );
    codex_cmd(
        &mut a,
        "i2",
        &format!(
            "/bin/bash -lc 'ls {}'",
            home.join("skills").to_string_lossy()
        ),
        0,
    );
    assert_eq!(signals(&seen), vec![pair("opened", "wicked-garden-domain")]);
    assert_eq!(seen.lock().unwrap()[0].to_json()["how"], "shell");
    // `$CODEX_HOME` as THIS launch set it, whatever the daemon's own environment says.
    codex_cmd(
        &mut a,
        "i3",
        "/bin/bash -lc 'cat \"$CODEX_HOME/skills/wicked-garden-qe/SKILL.md\"'",
        0,
    );
    // A non-portable skill was never populated into codex's home.
    codex_cmd(
        &mut a,
        "i4",
        &format!(
            "cat {}",
            home.join("skills/wicked-garden-claude-only/SKILL.md")
                .to_string_lossy()
        ),
        0,
    );
    assert_eq!(
        signals(&seen),
        vec![
            pair("opened", "wicked-garden-domain"),
            pair("opened", "wicked-garden-qe")
        ]
    );
}

#[test]
fn codex_running_a_garden_script_is_ran_even_when_it_exits_nonzero() {
    let (snap, base) = fixture();
    let (mut a, seen) = codex(&snap, &base.join("codex-home"));
    codex_cmd(
        &mut a,
        "i1",
        "/bin/bash -lc 'wicked-garden run scripts/qe/prove.py --run abc'",
        1,
    );
    assert_eq!(signals(&seen), vec![pair("ran", "scripts/qe/prove.py")]);
}

/// The live frame's target is clipped at 160 characters; usage reads the whole path.
// A POSIX shell line: a Windows host path in one is itself a run of escapes, so these run on Unix.
#[cfg(unix)]
#[test]
fn a_path_longer_than_the_frame_clip_is_still_opened() {
    let (snap, base) = fixture();
    let deep = base.join(format!("codex-{}", "h".repeat(170)));
    let (mut a, seen) = codex(&snap, &deep);
    let file = deep.join("skills/wicked-garden-qe/SKILL.md");
    assert!(file.to_string_lossy().len() > crate::tool_activity::TARGET_MAX);
    codex_cmd(&mut a, "i1", &format!("cat {}", file.to_string_lossy()), 0);
    assert_eq!(signals(&seen), vec![pair("opened", "wicked-garden-qe")]);
}

// ── pi ─────────────────────────────────────────────────────────────────────────────────────────

#[test]
fn pi_read_tool_opens_and_bash_runs() {
    let (snap, _b) = fixture();
    let d = snap.delivery_with(&cli("pi", SkillsLever::PiSkillFlags), false);
    let (s, seen) = sink_for(&snap, &d, None, "pi", &session());
    let mut a = PiJson::default().with_skill_use(Some(s));
    for (id, name, args) in [
        (
            "c1",
            "read",
            serde_json::json!({"path": skill_file(&snap, "domain")}),
        ),
        (
            "c2",
            "bash",
            serde_json::json!({"command": "wicked-garden run scripts/domain/extract_loop.py"}),
        ),
    ] {
        a.on_line(
            &serde_json::json!({"type": "tool_execution_start", "toolCallId": id,
                "toolName": name, "args": args})
            .to_string(),
        );
        a.on_line(
            &serde_json::json!({"type": "tool_execution_end", "toolCallId": id,
                "toolName": name, "isError": false})
            .to_string(),
        );
    }
    assert_eq!(
        signals(&seen),
        vec![
            pair("opened", "wicked-garden-domain"),
            pair("ran", "scripts/domain/extract_loop.py")
        ]
    );
}

// ── opencode ───────────────────────────────────────────────────────────────────────────────────

fn opencode_tool(tool: &str, status: &str, input: serde_json::Value) -> String {
    serde_json::json!({"type": "tool_use", "part": {"tool": tool,
        "state": {"status": status, "input": input}}})
    .to_string()
}

#[test]
fn opencode_skill_tool_and_read_open_and_bash_runs() {
    let (snap, _b) = fixture();
    let d = snap.delivery_with(&cli("opencode", SkillsLever::OpencodeConfig), false);
    let (s, seen) = sink_for(&snap, &d, None, "opencode", &session());
    let mut a = OpencodeJson::with_skill_use(Some(s));
    a.on_line(&opencode_tool(
        "skill",
        "completed",
        serde_json::json!({"name": "wicked-garden-qe"}),
    ));
    // A name its config never delivered (non-portable).
    a.on_line(&opencode_tool(
        "skill",
        "completed",
        serde_json::json!({"name": "wicked-garden-claude-only"}),
    ));
    a.on_line(&opencode_tool(
        "read",
        "completed",
        serde_json::json!({"filePath": skill_file(&snap, "domain")}),
    ));
    a.on_line(&opencode_tool(
        "bash",
        "completed",
        serde_json::json!({"command": "wicked-garden run scripts/qe/vault_gate.py"}),
    ));
    // A refused call loaded nothing.
    a.on_line(&opencode_tool(
        "read",
        "error",
        serde_json::json!({"filePath": skill_file(&snap, "a")}),
    ));
    assert_eq!(
        signals(&seen),
        vec![
            pair("opened", "wicked-garden-qe"),
            pair("opened", "wicked-garden-domain"),
            pair("ran", "scripts/qe/vault_gate.py")
        ]
    );
}

// ── agy ────────────────────────────────────────────────────────────────────────────────────────

fn agy_tool(name: &str, params: serde_json::Value) -> String {
    serde_json::json!({"event": "step_update", "step_update": {"step_index": 2, "state": "DONE",
        "step_type": "tool", "tool_name": name, "tool_info": {"name": name, "parameters": params}}})
    .to_string()
}

#[test]
fn agy_view_file_opens_and_run_command_runs() {
    let (snap, _b) = fixture();
    // agy has no lever: the launcher only, which still reaches the snapshot root.
    let d = snap.delivery_with(&cli("agy", SkillsLever::Absent), false);
    assert!(matches!(d, SkillsDelivery::LauncherOnly(_)));
    let (s, seen) = sink_for(&snap, &d, None, "agy", &session());
    let mut a = AgyStreamJson::with_skill_use(Some(s));
    a.on_line(&agy_tool(
        "view_file",
        serde_json::json!({"AbsolutePath": skill_file(&snap, "domain-extractor")}),
    ));
    a.on_line(&agy_tool(
        "run_command",
        serde_json::json!({"CommandLine": "wicked-garden run scripts/learn/check.py"}),
    ));
    assert_eq!(
        signals(&seen),
        vec![
            pair("opened", "wicked-garden-domain-extractor"),
            pair("ran", "scripts/learn/check.py")
        ]
    );
}

// ── guards ─────────────────────────────────────────────────────────────────────────────────────

// A POSIX shell line: a Windows host path in one is itself a run of escapes, so these run on Unix.
#[cfg(unix)]
#[test]
fn paths_that_are_not_a_handed_skill_file_being_read_are_not_opens() {
    let (snap, _b) = fixture();
    let (mut a, seen) = claude(&snap);
    // A run editing garden itself: its own tree has `skills/domain/SKILL.md`.
    for (id, p) in [
        ("r1", "/work/garden/skills/domain/SKILL.md".to_string()),
        ("r2", "skills/domain/SKILL.md".to_string()),
        (
            "r3",
            format!("{}/skills/domain/*.md", snap.root.to_string_lossy()),
        ),
        (
            "r4",
            format!("{}/skills/domain/missing.md", snap.root.to_string_lossy()),
        ),
    ] {
        claude_call(
            &mut a,
            id,
            "Read",
            serde_json::json!({ "file_path": p }),
            true,
        );
    }
    let r = snap.root.join("skills");
    let r = r.to_string_lossy();
    let file = skill_file(&snap, "domain");
    for (i, cmd) in [
        format!("ls {r}/domain"),
        format!("ls -l {file}"),
        format!("test -f {file} && echo yes"),
        format!("# cat {file}"),
        format!("echo {file}"),
        format!("rg -n foo {r}"),
        format!("cat {r}/*/SKILL.md"),
        format!("cat {r}/domain/[S]KILL.md"),
        format!("cat {r}/domain/refs.d"),
        "cat skills/domain/SKILL.md".to_string(),
    ]
    .iter()
    .enumerate()
    {
        bash(&mut a, &format!("b{i}"), cmd);
    }
    assert!(signals(&seen).is_empty(), "{:?}", signals(&seen));
    // An extensionless support file inside the skill dir is a read of it.
    bash(&mut a, "ok", &format!("cat {r}/domain/references/README"));
    assert_eq!(signals(&seen), vec![pair("opened", "wicked-garden-domain")]);
}

// A POSIX shell line: a Windows host path in one is itself a run of escapes, so these run on Unix.
#[cfg(unix)]
#[test]
fn only_a_real_launch_of_a_snapshot_script_is_ran() {
    let (snap, _b) = fixture();
    let (mut a, seen) = claude(&snap);
    let scripts = snap.root.join("scripts");
    let s = scripts.to_string_lossy();
    for (i, cmd) in [
        // Command data that merely mentions the launcher.
        "printf '%s\\n' wicked-garden run 'scripts/tool.py SECRET /home/alice/private'".to_string(),
        "echo wicked-garden run scripts/qe/prove.py".to_string(),
        // A script the snapshot does not have, or a path climbing out of it.
        "wicked-garden run scripts/nonexistent.py".to_string(),
        "wicked-garden run scripts/../../etc/passwd".to_string(),
        "wicked-garden run 'scripts/qe/prove.py SECRET'".to_string(),
        // Reading a script is not running it.
        format!("cat {s}/qe/prove.py"),
        // A launcher by that name somewhere else.
        "/work/tree/wicked-garden run scripts/qe/prove.py".to_string(),
    ]
    .iter()
    .enumerate()
    {
        bash(&mut a, &format!("n{i}"), cmd);
    }
    assert!(signals(&seen).is_empty(), "{:?}", signals(&seen));
    bash(
        &mut a,
        "y1",
        &format!("{s}/wicked-garden run scripts/qe/prove.py"),
    );
    bash(
        &mut a,
        "y2",
        &format!("node {s}/wicked-garden.mjs run scripts/qe/vault_gate.py"),
    );
    bash(&mut a, "y3", &format!("python3 {s}/learn/check.py --all"));
    assert_eq!(
        signals(&seen),
        vec![
            pair("ran", "scripts/qe/prove.py"),
            pair("ran", "scripts/qe/vault_gate.py"),
            pair("ran", "scripts/learn/check.py")
        ]
    );
}

// A POSIX shell line: a Windows host path in one is itself a run of escapes, so these run on Unix.
#[cfg(unix)]
#[test]
fn one_command_naming_more_than_three_skills_is_browsed_not_opened() {
    let (snap, _b) = fixture();
    let files: Vec<String> = ["a", "b", "c", "d"]
        .iter()
        .map(|d| skill_file(&snap, d))
        .collect();
    let (mut a, seen) = claude(&snap);
    bash(&mut a, "t1", &format!("head -5 {}", files.join(" ")));
    assert_eq!(
        signals(&seen),
        vec![
            pair("browsed", "wicked-garden-a"),
            pair("browsed", "wicked-garden-b"),
            pair("browsed", "wicked-garden-c"),
            pair("browsed", "wicked-garden-d")
        ]
    );
    // A loop names them too.
    let (mut a, seen) = claude(&snap);
    bash(
        &mut a,
        "t1",
        &format!("for f in {}; do head -3 \"$f\"; done", files.join(" ")),
    );
    assert_eq!(
        signals(&seen)
            .iter()
            .filter(|(s, _)| s == "browsed")
            .count(),
        4
    );
    // Exactly three is reading them: opened.
    let (mut a, seen) = claude(&snap);
    bash(&mut a, "t1", &format!("cat {}", files[..3].join(" ")));
    assert_eq!(
        signals(&seen).iter().filter(|(s, _)| s == "opened").count(),
        3
    );
}

#[test]
fn one_attempt_records_each_signal_once_across_relaunches() {
    let (snap, _b) = fixture();
    let d = snap.delivery_with(&WorkerCli::Claude, false);
    let sid = session();
    let mut all = Vec::new();
    // Two launches of the same unit attempt (a runtime-wait relaunch), each reading twice.
    for _ in 0..2 {
        let (s, seen) = sink_for(&snap, &d, None, "claude", &sid);
        let mut a = ClaudeStreamJson::default().with_skill_use(Some(s));
        for id in ["t1", "t2"] {
            claude_call(
                &mut a,
                id,
                "Read",
                serde_json::json!({"file_path": skill_file(&snap, "qe")}),
                true,
            );
        }
        all.extend(signals(&seen));
    }
    assert_eq!(all, vec![pair("opened", "wicked-garden-qe")]);
    // The run's end releases it.
    forget_run(&sid);
    let (s, seen) = sink_for(&snap, &d, None, "claude", &sid);
    let mut a = ClaudeStreamJson::default().with_skill_use(Some(s));
    claude_call(
        &mut a,
        "t3",
        "Read",
        serde_json::json!({"file_path": skill_file(&snap, "qe")}),
        true,
    );
    assert_eq!(signals(&seen).len(), 1);
}

#[test]
fn a_closed_stream_drops_unanswered_reads_and_keeps_started_scripts() {
    let (snap, _b) = fixture();
    let (mut a, seen) = claude(&snap);
    for (id, name, input) in [
        (
            "t1",
            "Read",
            serde_json::json!({"file_path": skill_file(&snap, "qe")}),
        ),
        (
            "t2",
            "Bash",
            serde_json::json!({"command": "wicked-garden run scripts/qe/prove.py"}),
        ),
    ] {
        a.on_line(
            &serde_json::json!({"type": "assistant", "message": {"id": "m1", "content": [
                {"type": "tool_use", "id": id, "name": name, "input": input}]}})
            .to_string(),
        );
    }
    assert!(signals(&seen).is_empty());
    a.finish();
    assert_eq!(signals(&seen), vec![pair("ran", "scripts/qe/prove.py")]);
}

/// The live frames drop calls past their window (12 per 2 s); usage must not.
#[test]
fn an_open_past_the_frame_rate_limit_is_still_recorded() {
    let (snap, base) = fixture();
    let frames: Seen = Arc::new(Mutex::new(Vec::new()));
    let ftx = Arc::clone(&frames);
    let sid = session();
    let activity = crate::tool_activity::ToolActivitySink::new(
        &sid,
        3,
        0,
        &base,
        Box::new(move |ev| ftx.lock().unwrap().push(ev)),
    );
    let d = snap.delivery_with(&WorkerCli::Claude, false);
    let (s, seen) = sink_for(&snap, &d, None, "claude", &sid);
    let mut a = ClaudeStreamJson::with_activity(Some(activity)).with_skill_use(Some(s));
    for i in 0..crate::tool_activity::WINDOW_CALLS {
        claude_call(
            &mut a,
            &format!("x{i}"),
            "Read",
            serde_json::json!({"file_path": format!("/work/tree/f{i}.rs")}),
            true,
        );
    }
    claude_call(
        &mut a,
        "last",
        "Read",
        serde_json::json!({"file_path": skill_file(&snap, "domain")}),
        true,
    );
    let shown = frames
        .lock()
        .unwrap()
        .iter()
        .filter(|e| matches!(e, CoreEvent::ToolActivity { id, .. } if id == "last"))
        .count();
    assert_eq!(shown, 0, "the frame limit dropped the last call");
    assert_eq!(signals(&seen), vec![pair("opened", "wicked-garden-domain")]);
}

// A POSIX shell line: a Windows host path in one is itself a run of escapes, so these run on Unix.
#[cfg(unix)]
#[test]
fn no_emitted_record_carries_a_host_path_or_an_argument() {
    let (snap, base) = fixture();
    let home = base.join("codex-home");
    let (mut a, seen) = codex(&snap, &home);
    codex_cmd(
        &mut a,
        "i1",
        &format!(
            "/bin/bash -lc 'cat {} && python3 {}/scripts/qe/prove.py --label hidden-value'",
            home.join("skills/wicked-garden-qe/SKILL.md")
                .to_string_lossy(),
            snap.root.to_string_lossy()
        ),
        0,
    );
    assert_eq!(
        signals(&seen),
        vec![
            pair("opened", "wicked-garden-qe"),
            pair("ran", "scripts/qe/prove.py")
        ]
    );
    for e in seen.lock().unwrap().iter() {
        let j = e.to_json().to_string();
        let base_s = base.to_string_lossy();
        assert!(!j.contains(base_s.as_ref()), "{j}");
        assert!(!j.contains("hidden-value") && !j.contains("--label"), "{j}");
    }
}

/// The runner's own construction: the sink sends on the runner's event channel.
#[test]
fn the_launch_sink_sends_on_the_runner_channel() {
    let (snap, _b) = fixture();
    let d = snap.delivery_with(&WorkerCli::Claude, false);
    let paths = Arc::new(SkillPaths::new(&snap, &d, None));
    let input = input_for(crate::domain::WorkUnit::pending("s:x", "s", 3, "X"));
    assert!(SkillUseSink::for_launch(&input, "claude", None, Some(paths.clone()), None).is_none());
    let (tx, rx) = std::sync::mpsc::channel();
    let sink = SkillUseSink::for_launch(&input, "claude", Some(&snap), Some(paths), Some(tx));
    let mut a = ClaudeStreamJson::default().with_skill_use(sink);
    claude_call(
        &mut a,
        "t1",
        "Skill",
        serde_json::json!({"skill": "wicked-garden:qe"}),
        true,
    );
    match rx.try_recv() {
        Ok(crate::command::Command::EmitEvent(ev)) => {
            assert_eq!(ev.to_json()["skill"], "wicked-garden-qe");
            assert_eq!(ev.to_json()["session"], input.run_id.as_str());
        }
        _ => panic!("no skillUsed on the runner channel"),
    }
}

// ── directed and handed ────────────────────────────────────────────────────────────────────────

fn input_for(unit: crate::domain::WorkUnit) -> crate::workflow::StepInput {
    crate::workflow::StepInput {
        run_id: session(),
        unit_ix: 0,
        attempt: 1,
        unit,
        workflow_id: "wf".to_string(),
        entity_mode: crate::scope::EntityMode::Isolated,
        workdir: None,
        governance: None,
        prior_outputs: vec![],
        elicitation_epoch: 0,
        process_gen: None,
        launch_seq: 0,
        required_skills: Vec::new(),
    }
}

#[test]
fn the_directive_names_phase_base_and_required_qe_skills_once_per_attempt() {
    let (snap, _b) = fixture();
    let mut unit = crate::domain::WorkUnit::pending("s:extract", "s", 2, "Extract");
    unit.skill_ref = Some("wicked-garden-domain-extractor".to_string());
    unit.base_skill_ref = Some("wicked-garden-domain".to_string());
    unit.qe_acceptance = Some(crate::assurance::QeAcceptance {
        status: crate::assurance::QE_REQUIRED.into(),
        basis: crate::assurance::QE_BASIS_DIFF.into(),
        score: Some(60),
        threshold: 20,
        reason: "required".into(),
        reasons: Vec::new(),
        ord: Some(2),
        tree: None,
    });
    let input = input_for(unit.clone());
    let evs = directed_events(&input, "codex", Some(&snap));
    let got: Vec<(String, String)> = evs
        .iter()
        .map(|e| {
            let j = e.to_json();
            (
                j["skill"].as_str().unwrap().to_string(),
                j["via"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            pair("wicked-garden-domain-extractor", "phase"),
            pair("wicked-garden-domain", "base"),
            pair("wicked-garden-qe", "qe")
        ]
    );
    assert!(evs.iter().all(|e| e.to_json()["signal"] == "directed"));
    assert!(evs.iter().all(|e| e.to_json()["gen"] == "000042"));
    // A relaunch of the same attempt directs nothing new.
    assert!(directed_events(&input, "codex", Some(&snap)).is_empty());
    // A waived QE unit is told NOT to run it: not directed.
    unit.qe_acceptance.as_mut().unwrap().status = crate::assurance::QE_WAIVED.into();
    assert_eq!(
        directed_events(&input_for(unit), "codex", Some(&snap)).len(),
        2
    );
    // An engine-internal judge with no skill is directed to nothing (its base is not spelled).
    let mut judge = crate::domain::WorkUnit::pending("validator:1", "validator", 1, "Judge");
    judge.base_skill_ref = Some("wicked-garden-domain".to_string());
    assert!(directed_events(&input_for(judge), "claude", Some(&snap)).is_empty());
}

#[test]
fn handed_names_follow_the_lever() {
    let (snap, _b) = fixture();
    let claude = snap.handed_names(&snap.delivery_with(&WorkerCli::Claude, false));
    assert!(claude.contains(&"wicked-garden-claude-only".to_string()));
    assert!(
        !claude.contains(&"wicked-garden-qe-oracle".to_string()),
        "nested is not invocable"
    );
    let codex_d = snap.delivery_with(&cli("codex", SkillsLever::CodexSkillsDir), false);
    let codex = snap.handed_names(&codex_d);
    let mut portable: Vec<String> = snap
        .deliverable_portable()
        .into_iter()
        .map(|s| s.name.clone())
        .collect();
    portable.sort();
    assert_eq!(codex, portable);
    assert!(!codex.contains(&"wicked-garden-claude-only".to_string()));
    for (key, lever) in [
        ("pi", SkillsLever::PiSkillFlags),
        ("opencode", SkillsLever::OpencodeConfig),
    ] {
        assert_eq!(
            snap.handed_names(&snap.delivery_with(&cli(key, lever), false)),
            portable,
            "{key}"
        );
    }
    let agy = snap.handed_names(&snap.delivery_with(&cli("agy", SkillsLever::Absent), false));
    assert!(agy.is_empty());
    let ev = snap.handed_event("run-1", 1, 0, "wrapped_cli", "codex", &codex_d);
    let j = ev.to_json();
    assert_eq!(j["lever"], "codex_skills_dir");
    assert_eq!(j["skills"].as_array().unwrap().len(), portable.len());
}

// ── review r2: shell structure ─────────────────────────────────────────────────────────────────

// A POSIX shell line: a Windows host path in one is itself a run of escapes, so these run on Unix.
#[cfg(unix)]
#[test]
fn shell_boundaries_quotes_and_comments_are_the_shells() {
    let (snap, _b) = fixture();
    let f = skill_file(&snap, "domain");
    // Quoted punctuation is data; a newline ends a command; `bash -lc` keeps its script whole;
    // a quoted ` #` is not a comment.
    for (cmd, want) in [
        (format!("printf '%s\\n' ';' cat {f}"), false),
        (format!("echo ready\ncat {f}"), true),
        (format!("bash -lc 'ls /tmp; cat {f}'"), true),
        (format!("printf ' #'; cat {f}"), true),
        (format!("cat '{f}"), false),
    ] {
        let (mut a, seen) = claude(&snap);
        bash(&mut a, "t", &cmd);
        let got = signals(&seen) == vec![pair("opened", "wicked-garden-domain")];
        assert_eq!(got, want, "{cmd:?} -> {:?}", signals(&seen));
    }
}

// A POSIX shell line: a Windows host path in one is itself a run of escapes, so these run on Unix.
#[cfg(unix)]
#[test]
fn only_reading_programs_open_and_every_named_file_counts_toward_browse() {
    let (snap, _b) = fixture();
    let f = skill_file(&snap, "domain");
    for cmd in [
        format!("rg --files {f}"),
        format!("true {f}"),
        format!(": {f}"),
    ] {
        let (mut a, seen) = claude(&snap);
        bash(&mut a, "t", &cmd);
        assert!(signals(&seen).is_empty(), "{cmd}: {:?}", signals(&seen));
    }
    // A listing beside a read still names four skills: browsed.
    let [fa, fb, fc, fd] = ["a", "b", "c", "d"].map(|d| skill_file(&snap, d));
    let (mut a, seen) = claude(&snap);
    bash(&mut a, "t", &format!("ls {fb} {fc} {fd}; cat {fa}"));
    assert_eq!(
        signals(&seen)
            .iter()
            .filter(|(s, _)| s == "browsed")
            .count(),
        4
    );
    assert!(!signals(&seen).iter().any(|(s, _)| s == "opened"));
}

// A POSIX shell line: a Windows host path in one is itself a run of escapes, so these run on Unix.
#[cfg(unix)]
#[test]
fn interpreter_forms_and_assignments() {
    let (snap, _b) = fixture();
    let s = snap.root.join("scripts");
    let s = s.to_string_lossy();
    let (mut a, seen) = claude(&snap);
    bash(&mut a, "n1", &format!("python3 --help {s}/qe/prove.py"));
    assert!(signals(&seen).is_empty(), "{:?}", signals(&seen));
    bash(&mut a, "y1", &format!("deno run {s}/qe/vault_gate.py"));
    bash(
        &mut a,
        "y2",
        &format!("PYTHONPATH=/somewhere python3 {s}/learn/check.py"),
    );
    assert_eq!(
        signals(&seen),
        vec![
            pair("ran", "scripts/qe/vault_gate.py"),
            pair("ran", "scripts/learn/check.py")
        ]
    );
}

#[test]
fn a_single_quoted_variable_is_literal() {
    let (snap, base) = fixture();
    let (mut a, seen) = codex(&snap, &base.join("codex-home"));
    codex_cmd(
        &mut a,
        "i1",
        "/bin/bash -lc \"cat '\\$CODEX_HOME/skills/wicked-garden-qe/SKILL.md'\"",
        0,
    );
    assert!(signals(&seen).is_empty(), "{:?}", signals(&seen));
}

#[test]
fn agy_reads_only_through_view_file() {
    let (snap, _b) = fixture();
    let d = snap.delivery_with(&cli("agy", SkillsLever::Absent), false);
    let (s, seen) = sink_for(&snap, &d, None, "agy", &session());
    let mut a = AgyStreamJson::with_skill_use(Some(s));
    a.on_line(&agy_tool(
        "write_to_file",
        serde_json::json!({"AbsolutePath": skill_file(&snap, "domain")}),
    ));
    assert!(signals(&seen).is_empty());
}

#[test]
fn a_huge_call_id_is_held_bounded_and_still_matched() {
    let (snap, _b) = fixture();
    let (mut a, seen) = claude(&snap);
    let id = "é".repeat(400);
    claude_call(
        &mut a,
        &id,
        "Read",
        serde_json::json!({"file_path": skill_file(&snap, "qe")}),
        true,
    );
    assert_eq!(signals(&seen), vec![pair("opened", "wicked-garden-qe")]);
}

// ── review r3 ──────────────────────────────────────────────────────────────────────────────────

// A POSIX shell line: a Windows host path in one is itself a run of escapes, so these run on Unix.
#[cfg(unix)]
#[test]
fn reads_inside_shell_control_flow_count() {
    let (snap, _b) = fixture();
    let f = skill_file(&snap, "qe");
    for cmd in [
        format!("if test -f {f}; then cat {f}; fi"),
        format!("for i in 1; do cat {f}; done"),
        format!("{{ cat {f}; }}"),
    ] {
        let (mut a, seen) = claude(&snap);
        bash(&mut a, "t", &cmd);
        assert_eq!(
            signals(&seen),
            vec![pair("opened", "wicked-garden-qe")],
            "{cmd}"
        );
    }
}

// A POSIX shell line: a Windows host path in one is itself a run of escapes, so these run on Unix.
#[cfg(unix)]
#[test]
fn a_scripts_own_help_argument_is_still_a_run() {
    let (snap, _b) = fixture();
    let s = snap.root.join("scripts");
    let s = s.to_string_lossy();
    let (mut a, seen) = claude(&snap);
    bash(&mut a, "y1", &format!("python3 {s}/qe/prove.py --help"));
    bash(
        &mut a,
        "y2",
        &format!("node {s}/wicked-garden.mjs run scripts/learn/check.py --help"),
    );
    assert_eq!(
        signals(&seen),
        vec![
            pair("ran", "scripts/qe/prove.py"),
            pair("ran", "scripts/learn/check.py")
        ]
    );
}
