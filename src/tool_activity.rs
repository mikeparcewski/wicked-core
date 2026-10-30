//! Live per-tool activity from a seat's stream (program DES-POWER-MODE, operator ask 2026-10-30:
//! "live progress while a step runs, like Claude Code / Codex").
//!
//! The governance hook records and `toolInvoked` land when a unit ends; on rig run b6f7968b all 26
//! of a 20-minute turn's tool calls arrived at once at its end. This module turns the seat's own
//! stream (claude `stream-json`: a `tool_use` block when a call starts, the `tool_result` that
//! answers it when it finishes) into one `toolActivity` frame per state change, sent to the run's
//! actor as it happens. The frame shape is the one power-mode-ui renders:
//! `{session, ord, attempt, at, id, state, kind, tool, target, ok, durationMs}`.
//!
//! A target is a repo-relative path or a redacted command, at most 160 characters. Nothing here
//! decides anything: a frame that cannot be sent is dropped.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::Instant;

use serde_json::Value;

use crate::command::Command;
use crate::event::CoreEvent;

/// The longest target a frame carries.
pub const TARGET_MAX: usize = 160;
/// Calls one unit attempt may show starting in a [`WINDOW_MS`] window: a burst of parallel reads is
/// cut. Each shown call also gets its finish, whenever it comes, so a window carries at most
/// twice this many frames of its own calls.
pub const WINDOW_CALLS: usize = 12;
pub const WINDOW_MS: u128 = 2_000;
/// Calls one unit attempt may show in all, across every turn of a multi-turn orchestrator (a
/// looping seat cannot flood the log): at most twice this many frames.
pub const MAX_CALLS: usize = 2_000;

struct Open {
    at: Instant,
    kind: &'static str,
    tool: String,
    target: Option<String>,
}

/// Calls shown per unit attempt, shared by every sink of that attempt (each orchestrator turn
/// builds its own), released when the run ends ([`forget_run`]).
type Budget = std::sync::Mutex<HashMap<(String, u32, u32), usize>>;

fn shown() -> &'static Budget {
    static S: std::sync::OnceLock<Budget> = std::sync::OnceLock::new();
    S.get_or_init(Default::default)
}

/// Forget a run's call budget (its end).
pub fn forget_run(run_id: &str) {
    shown()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retain(|(r, _, _), _| r != run_id);
}

/// One unit attempt's live tool frames.
pub struct ToolActivitySink {
    run_id: String,
    ord: u32,
    attempt: u32,
    root: PathBuf,
    tx: Box<dyn Fn(CoreEvent) + Send>,
    open: HashMap<String, Open>,
    window: (Instant, usize),
}

impl ToolActivitySink {
    /// The sink for a dispatched unit, `None` when its actor is not reachable from here.
    pub fn for_input(input: &crate::workflow::StepInput, root: &Path) -> Option<Self> {
        let tx: Sender<Command> = crate::power::actor_tx(input.process_gen)?;
        let tx = std::sync::Mutex::new(tx);
        Some(Self::new(
            &input.run_id,
            input.unit.ord,
            input.attempt,
            root,
            Box::new(move |ev| {
                if let Ok(t) = tx.lock() {
                    let _ = t.send(Command::EmitEvent(ev));
                }
            }),
        ))
    }

    pub fn new(
        run_id: &str,
        ord: u32,
        attempt: u32,
        root: &Path,
        tx: Box<dyn Fn(CoreEvent) + Send>,
    ) -> Self {
        Self {
            run_id: run_id.to_string(),
            ord,
            attempt,
            root: root.to_path_buf(),
            tx,
            open: HashMap::new(),
            window: (Instant::now(), 0),
        }
    }

    /// Room to show one more call now. A shown call's finish needs no room: it was reserved.
    fn room(&mut self) -> bool {
        if self.window.0.elapsed().as_millis() >= WINDOW_MS {
            self.window = (Instant::now(), 0);
        }
        if self.window.1 >= WINDOW_CALLS {
            return false;
        }
        let mut s = shown().lock().unwrap_or_else(|p| p.into_inner());
        let n = s
            .entry((self.run_id.clone(), self.ord, self.attempt))
            .or_insert(0);
        if *n >= MAX_CALLS {
            return false;
        }
        *n += 1;
        self.window.1 += 1;
        true
    }

    /// A `tool_use` block: the call `id` of `tool` with `input` started.
    pub fn started(&mut self, id: &str, tool: &str, input: &Value) {
        if id.is_empty() || self.open.contains_key(id) || !self.room() {
            return;
        }
        let kind = kind_of(tool, input);
        let target = target_of(tool, input, &self.root);
        (self.tx)(self.frame(id, "started", kind, tool, target.clone(), None, None));
        self.open.insert(
            id.to_string(),
            Open {
                at: Instant::now(),
                kind,
                tool: tool.to_string(),
                target,
            },
        );
    }

    /// A `tool_result` block answered call `id` (`ok`: it was not an error). Only a call shown
    /// starting is shown finishing, and always.
    pub fn finished(&mut self, id: &str, ok: bool) {
        let Some(o) = self.open.remove(id) else {
            return;
        };
        let ms = o.at.elapsed().as_millis() as u64;
        (self.tx)(self.frame(
            id,
            "finished",
            o.kind,
            &o.tool,
            o.target,
            Some(ok),
            Some(ms),
        ));
    }

    /// The seat's stream closed (it ended, crashed, timed out or was cancelled): every call still
    /// open is shown finishing, not ok, so no subscriber is left holding a started call.
    pub fn close(&mut self) {
        let mut ids: Vec<String> = self.open.keys().cloned().collect();
        ids.sort();
        for id in ids {
            self.finished(&id, false);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn frame(
        &self,
        id: &str,
        state: &str,
        kind: &str,
        tool: &str,
        target: Option<String>,
        ok: Option<bool>,
        duration_ms: Option<u64>,
    ) -> CoreEvent {
        CoreEvent::ToolActivity {
            session: self.run_id.clone(),
            ord: self.ord,
            attempt: self.attempt,
            at: crate::interaction::now_millis(),
            id: id.to_string(),
            state: state.to_string(),
            kind: kind.to_string(),
            tool: tool.to_string(),
            target,
            ok,
            duration_ms,
        }
    }
}

/// Words that make a shell command a test run.
const TEST_WORDS: [&str; 9] = [
    "cargo test",
    "cargo nextest",
    "vitest",
    "jest",
    "pytest",
    "npm test",
    "npm run test",
    "go test",
    "playwright test",
];

/// What a call does, in the frame's words.
pub fn kind_of(tool: &str, input: &Value) -> &'static str {
    match tool {
        "Read" | "NotebookRead" => "read",
        "Grep" | "Glob" | "LS" | "WebSearch" | "WebFetch" | "ToolSearch" => "search",
        "Edit" | "MultiEdit" | "NotebookEdit" => "edit",
        "Write" => "write",
        "Skill" => "skill",
        "Task" | "Agent" => "task",
        "Bash" | "BashOutput" => {
            let cmd = input.get("command").and_then(Value::as_str).unwrap_or("");
            if TEST_WORDS.iter().any(|w| cmd.contains(w)) {
                "test"
            } else {
                "run"
            }
        }
        _ => "other",
    }
}

/// What a call works on: a path made relative to `root`, a search pattern, or a redacted
/// command with its paths made relative too; at most [`TARGET_MAX`] characters.
pub fn target_of(tool: &str, input: &Value, root: &Path) -> Option<String> {
    let s = |k: &str| {
        input
            .get(k)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
    };
    let raw = match tool {
        "Bash" => s("command").map(|c| redact(&paths_in(c, root))),
        "Grep" | "Glob" => match (s("pattern"), s("path")) {
            (Some(p), Some(at)) => Some(format!("{p} in {}", relative(at, root))),
            (Some(p), None) => Some(p.to_string()),
            (None, at) => at.map(|a| relative(a, root)),
        },
        "Skill" => s("skill").or_else(|| s("name")).map(str::to_string),
        "Task" | "Agent" => s("description").map(str::to_string),
        "WebSearch" => s("query").map(str::to_string),
        "WebFetch" => s("url").map(redact),
        _ => s("file_path")
            .or_else(|| s("notebook_path"))
            .or_else(|| s("path"))
            .map(|p| relative(p, root)),
    }?;
    Some(clip(&raw))
}

fn home() -> Option<String> {
    std::env::var("HOME").ok().filter(|h| h.len() > 1)
}

/// `p` relative to `root` when it is inside it; a path outside it keeps only `~/` for the home
/// directory (no user name on the wire).
fn relative(p: &str, root: &Path) -> String {
    let path = Path::new(p);
    if let Ok(rest) = path.strip_prefix(root) {
        let r = rest.to_string_lossy().to_string();
        return if r.is_empty() { ".".into() } else { r };
    }
    if let Some(h) = home() {
        if let Ok(rest) = path.strip_prefix(Path::new(&h)) {
            return format!("~/{}", rest.to_string_lossy());
        }
    }
    p.to_string()
}

/// A command with the worktree's own paths made relative and the home directory shown as `~`
/// (review on #1195: a `cat` of a file under the home directory carried the user name).
fn paths_in(cmd: &str, root: &Path) -> String {
    let mut out = cmd.to_string();
    let r = root.to_string_lossy().trim_end_matches('/').to_string();
    if r.len() > 1 {
        out = out.replace(&format!("{r}/"), "").replace(&r, ".");
    }
    if let Some(h) = home() {
        out = out.replace(h.trim_end_matches('/'), "~");
    }
    out
}

/// A value as a shell word: single- or double-quoted (no escapes), or bare.
const VALUE: &str = r#"(?:'[^']*'|"[^"]*"|[^\s'"]+)"#;
/// A name that says it holds a secret.
const SECRET_NAME: &str = r"[A-Za-z0-9_.-]*(?i:token|secret|passw(?:or)?d|api[_-]?key|private[_-]?key|access[_-]?key|credential|auth)[A-Za-z0-9_.-]*";

fn redactors() -> &'static [(regex::Regex, &'static str)] {
    static R: std::sync::OnceLock<Vec<(regex::Regex, &'static str)>> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        let re = |p: String| regex::Regex::new(&p).expect("redaction pattern");
        vec![
            // An auth header's value, quoted or not, with or without a scheme word.
            (
                re(r#"(?i)((?:proxy-)?authorization:\s*(?:bearer|basic|token|digest)?\s*)[^\s'"]+"#.into()),
                "${1}***",
            ),
            (re(r#"(?i)\b(bearer\s+)[^\s'"]+"#.into()), "${1}***"),
            // Other secret-named headers (`X-Api-Key: …`, `Cookie: …`).
            (
                re(format!(r#"(?i)(\b(?:{SECRET_NAME}|cookie|x-api-key):\s*)[^\s'"]+"#)),
                "${1}***",
            ),
            // `NAME=value` and `--name=value` / `--name value` / `-p value` where the name says secret.
            (re(format!(r"(\b{SECRET_NAME}=){VALUE}")), "${1}***"),
            (re(format!(r"((?:^|\s)--?{SECRET_NAME}\s+){VALUE}")), "${1}***"),
            (re(format!(r"((?:^|\s)-(?:u|p)\s+){VALUE}")), "${1}***"),
            (re(format!(r"((?:^|\s)--user\s+){VALUE}")), "${1}***"),
            // Credentials in a URL.
            (re(r#"(://)[^/\s:@'"]+:[^/\s@'"]+@"#.into()), "${1}***@"),
        ]
    })
}

/// A command with secrets cut, quoted or not: auth header values, secret-named assignments, headers
/// and flags, `-u` / `-p` / `--user` values, and credentials in a URL.
pub fn redact(cmd: &str) -> String {
    let mut out = cmd.to_string();
    for (re, with) in redactors() {
        out = re.replace_all(&out, *with).into_owned();
    }
    out
}

fn clip(s: &str) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= TARGET_MAX {
        return one_line;
    }
    let mut cut: String = one_line.chars().take(TARGET_MAX - 1).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    fn sink(root: &str, run: &str) -> (ToolActivitySink, Arc<Mutex<Vec<Value>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = Arc::clone(&seen);
        let s = ToolActivitySink::new(
            run,
            3,
            0,
            Path::new(root),
            Box::new(move |ev| s2.lock().unwrap().push(ev.to_json())),
        );
        (s, seen)
    }

    #[test]
    fn a_call_is_one_started_and_one_finished_frame_in_the_ui_shape() {
        let (mut s, seen) = sink("/w/repo", "run-1");
        s.started(
            "t1",
            "Read",
            &json!({"file_path": "/w/repo/src/api/routes.ts"}),
        );
        s.finished("t1", true);
        s.finished("t1", true); // a second answer for the same call is ignored
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "{seen:?}");
        let a = &seen[0];
        assert_eq!(a["type"], "toolActivity");
        assert_eq!(a["session"], "run-1");
        assert_eq!(a["ord"], 3);
        assert_eq!(a["attempt"], 0);
        assert_eq!(a["id"], "t1");
        assert_eq!(a["state"], "started");
        assert_eq!(a["kind"], "read");
        assert_eq!(a["tool"], "Read");
        assert_eq!(a["target"], "src/api/routes.ts");
        assert!(a["ok"].is_null() && a["durationMs"].is_null(), "{a}");
        assert!(a["at"].as_i64().unwrap() > 0);
        let b = &seen[1];
        assert_eq!(b["state"], "finished");
        assert_eq!(b["kind"], "read");
        assert_eq!(b["target"], "src/api/routes.ts");
        assert_eq!(b["ok"], true);
        assert!(b["durationMs"].as_u64().is_some(), "{b}");
    }

    /// The claude stream adapter drives the sink live: a `tool_use` line starts a call, the
    /// `tool_result` that answers it (by id) finishes it, an error result is `ok: false`.
    #[test]
    fn the_claude_stream_sends_a_frame_per_line_as_it_arrives() {
        use crate::execute_wrapped::{ClaudeStreamJson, OutputAdapter};
        let (s, seen) = sink("/w/repo", "run-1");
        let mut a = ClaudeStreamJson::with_activity(Some(s));
        a.on_line(r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"tA","name":"Bash","input":{"command":"npx vitest run tests/x.test.ts"}}]}}"#);
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "the start is sent before the call returns"
        );
        a.on_line(r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tA","is_error":true,"content":"1 failed"}]}}"#);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert_eq!(seen[0]["kind"], "test");
        assert_eq!(seen[0]["target"], "npx vitest run tests/x.test.ts");
        assert_eq!(seen[1]["state"], "finished");
        assert_eq!(seen[1]["ok"], false);
    }

    #[test]
    fn kinds_follow_the_tool_and_a_test_command_is_a_test() {
        assert_eq!(
            kind_of(
                "Bash",
                &json!({"command": "npx vitest run tests/a.test.ts"})
            ),
            "test"
        );
        assert_eq!(
            kind_of("Bash", &json!({"command": "cargo test -p x"})),
            "test"
        );
        assert_eq!(kind_of("Bash", &json!({"command": "git status"})), "run");
        assert_eq!(kind_of("Grep", &json!({})), "search");
        assert_eq!(kind_of("Glob", &json!({})), "search");
        assert_eq!(kind_of("Edit", &json!({})), "edit");
        assert_eq!(kind_of("MultiEdit", &json!({})), "edit");
        assert_eq!(kind_of("Write", &json!({})), "write");
        assert_eq!(kind_of("Skill", &json!({})), "skill");
        assert_eq!(kind_of("Task", &json!({})), "task");
        assert_eq!(kind_of("mcp__x__y", &json!({})), "other");
    }

    #[test]
    fn targets_are_relative_redacted_and_clipped() {
        let root = Path::new("/w/repo");
        assert_eq!(
            target_of("Edit", &json!({"file_path": "/w/repo/a/b.rs"}), root).as_deref(),
            Some("a/b.rs")
        );
        assert_eq!(
            target_of(
                "Grep",
                &json!({"pattern": "fn x", "path": "/w/repo/src"}),
                root
            )
            .as_deref(),
            Some("fn x in src")
        );
        let bash = |c: &str| target_of("Bash", &json!({ "command": c }), root).unwrap();
        // Review on #1195: quoted values are the common case. The secrets are built at run time so
        // the source carries nothing secret-shaped (secret scanners read test files too).
        let v = |tag: &str| format!("{tag}{}", "Q7".repeat(4));
        let cases: Vec<(String, String)> = vec![
            (format!("GH_TOKEN={} curl https://h", v("a")), v("a")),
            (
                format!("curl -H 'Authorization: Basic {}' https://h", v("b")),
                v("b"),
            ),
            (
                format!("curl -H \"Authorization: token {}\" https://h", v("c")),
                v("c"),
            ),
            (
                format!("curl -H 'Authorization: {}' https://h", v("d")),
                v("d"),
            ),
            (
                format!("curl -H Authorization: Bearer {} https://h", v("e")),
                v("e"),
            ),
            (
                format!("PASSWORD='{} horse battery' ./check", v("f")),
                "horse".to_string(),
            ),
            (format!("tool --password {} run", v("g")), v("g")),
            (format!("tool --api-key={} run", v("h")), v("h")),
            (format!("curl -u alice:{} https://h", v("i")), v("i")),
            (
                format!("git clone https://bob:{}@example.test/x/y", v("j")),
                v("j"),
            ),
            (format!("curl -H 'X-Api-Key: {}' https://h", v("k")), v("k")),
        ];
        for (cmd, secret) in &cases {
            let t = bash(cmd);
            assert!(!t.contains(secret.as_str()), "{cmd:?} -> {t:?}");
            assert!(t.contains("***"), "{cmd:?} -> {t:?}");
        }
        assert_eq!(
            bash("git status --short"),
            "git status --short",
            "nothing secret, nothing cut"
        );
        // Paths in a command: the worktree's are relative, the home directory is `~`.
        assert_eq!(bash("cat /w/repo/src/a.rs"), "cat src/a.rs");
        if let Some(h) = home() {
            let t = bash(&format!("cat {h}/.config/app/settings"));
            assert_eq!(t, "cat ~/.config/app/settings");
        }
        let long = "x ".repeat(200);
        let t = target_of("Bash", &json!({ "command": long }), root).unwrap();
        assert_eq!(t.chars().count(), TARGET_MAX);
        assert!(t.ends_with('…'));
        assert_eq!(target_of("Bash", &json!({}), root), None);
    }

    #[test]
    fn a_burst_is_cut_but_a_shown_start_always_finishes() {
        let (mut s, seen) = sink("/w", "burst-run");
        for i in 0..(WINDOW_CALLS + 10) {
            s.started(&format!("t{i}"), "Read", &json!({"file_path": "/w/a"}));
        }
        assert_eq!(
            seen.lock().unwrap().len(),
            WINDOW_CALLS,
            "starts beyond the window are dropped"
        );
        for i in 0..(WINDOW_CALLS + 10) {
            s.finished(&format!("t{i}"), true);
        }
        let all = seen.lock().unwrap();
        let finished = all.iter().filter(|f| f["state"] == "finished").count();
        assert_eq!(
            finished, WINDOW_CALLS,
            "every shown start finishes, no unshown one does"
        );
    }

    /// Review on #1195: the attempt's budget is shared by every turn's sink, and a shown call's
    /// finish is reserved even when the budget is spent.
    #[test]
    fn the_attempt_budget_spans_turns_and_never_eats_a_finish() {
        let run = "budget-run";
        {
            let mut n = shown().lock().unwrap();
            n.insert((run.to_string(), 3, 0), MAX_CALLS - 1);
        }
        let (mut turn1, seen) = sink("/w", run);
        turn1.started("a", "Read", &json!({}));
        turn1.started("b", "Read", &json!({}));
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "the last call of the budget is shown, the next is not"
        );
        turn1.finished("a", true);
        assert_eq!(
            seen.lock().unwrap().len(),
            2,
            "its finish is shown though the budget is spent"
        );
        let (mut turn2, seen2) = sink("/w", run);
        turn2.started("c", "Read", &json!({}));
        assert!(
            seen2.lock().unwrap().is_empty(),
            "a new turn's sink shares the spent budget"
        );
        forget_run(run);
        turn2.started("d", "Read", &json!({}));
        assert_eq!(seen2.lock().unwrap().len(), 1, "the run's end releases it");
        forget_run(run);
    }

    /// Review on #1195: a stream that closes with calls open shows them finishing, not ok.
    #[test]
    fn a_closed_stream_finishes_every_open_call_not_ok() {
        let (mut s, seen) = sink("/w", "close-run");
        s.started("x", "Bash", &json!({"command": "sleep 100"}));
        s.started("y", "Read", &json!({"file_path": "/w/a"}));
        s.close();
        let seen = seen.lock().unwrap();
        let fin: Vec<_> = seen.iter().filter(|f| f["state"] == "finished").collect();
        assert_eq!(fin.len(), 2, "{seen:?}");
        assert!(fin.iter().all(|f| f["ok"] == false));
        forget_run("close-run");
    }
}
