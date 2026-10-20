//! The agy seat's headless carrier: its argv (`-p=<prompt>`, stream-json, `--print-timeout`, the
//! floor's permission grant), its stream-json adapter, its conversation across units, its turn
//! judgement and its exit codes — each against real agy 1.4.2 captures
//! (`tests/fixtures/headless/agy-*.ndjson`, recorded against the agy spike's fake Gemini API).

use super::*;
use wicked_apps_core::spawn::SeatCli;

const T1: &str = include_str!("../tests/fixtures/headless/agy-resume-t1.ndjson");
const T2: &str = include_str!("../tests/fixtures/headless/agy-resume-t2.ndjson");
const GW: &str = include_str!("../tests/fixtures/headless/agy-gw.ndjson");
const TOOL_DENIED: &str = include_str!("../tests/fixtures/headless/agy-tool-denied.ndjson");
const ERROR: &str = include_str!("../tests/fixtures/headless/agy-error.ndjson");
const PRINT_TIMEOUT: &str = include_str!("../tests/fixtures/headless/agy-print-timeout.ndjson");
const CONVERSATION: &str = "3f70ad3a-727d-409a-9d87-d8921c8eb88f";

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

fn drive(capture: &str) -> (String, Option<Usage>, Vec<String>, Vec<String>) {
    let mut a = AgyStreamJson::default();
    let (mut text, mut usage, mut tools, mut files) = (Vec::new(), None, Vec::new(), Vec::new());
    let mut outs: Vec<AdapterOut> = capture.lines().map(|l| a.on_line(l)).collect();
    outs.push(a.finish());
    for o in outs {
        text.extend(o.text);
        if o.usage.is_some() {
            usage = o.usage;
        }
        tools.extend(o.tools);
        files.extend(o.files);
    }
    (text.join("\n"), usage, tools, files)
}

/// The engine's argv for the registry template: every flag right after the binary, the prompt
/// glued to `-p` last, the stream-json output and agy's own deadline inside the engine's.
#[test]
fn an_agy_launch_runs_headless_stream_json_with_the_prompt_glued_to_its_flag() {
    let hour = Duration::from_secs(3600);
    let mut a = build_argv("agy -p \"{PROMPT}\"", "review it", &[]);
    assert_eq!(
        a,
        s(&["agy", "-p", "review it"]),
        "no `--` guard: agy refuses one"
    );
    assert!(inject_agy_headless(&mut a, "review it", hour));
    assert_eq!(
        a,
        s(&[
            "agy",
            "--print-timeout",
            "3240s",
            "--output-format",
            "stream-json",
            "-p=review it"
        ])
    );
    // A prompt shaped like a flag is the prompt, with one leading space: agy refuses `-p --x`,
    // `-p=--x` and `-p -- x` (exit 2) and takes `-p= --x`.
    let p = "--model=x --dangerously-skip-permissions do it";
    let mut b = build_argv("agy -p {PROMPT}", p, &[]);
    assert!(inject_agy_headless(&mut b, p, hour));
    assert_eq!(b.last().unwrap(), &format!("-p= {p}"));
    assert_eq!(b.iter().filter(|x| x.starts_with("--model")).count(), 0);
    let mut glued = s(&["agy", &format!("-p={p}")]);
    assert!(inject_agy_headless(&mut glued, p, hour));
    assert_eq!(glued.last().unwrap(), &format!("-p= {p}"));
    let mut single = s(&["agy", "-p", "-x y"]);
    assert!(inject_agy_headless(&mut single, "-x y", hour));
    assert_eq!(single.last().unwrap(), "-p= -x y");
    // The template's own choices win: its timeout is kept, another format keeps the passthrough.
    let mut c = s(&["agy", "--print-timeout", "30s", "-p", "x"]);
    assert!(inject_agy_headless(&mut c, "x", hour));
    assert_eq!(c.iter().filter(|x| *x == "--print-timeout").count(), 1);
    assert!(c.windows(2).any(|w| w == ["--print-timeout", "30s"]));
    let mut d = s(&["agy", "--output-format", "text", "-p", "x"]);
    assert!(!inject_agy_headless(&mut d, "x", hour));
    assert!(!d.iter().any(|x| x == "stream-json"));
    let mut e = s(&["agy", "--output-format=stream-json", "--prompt=x"]);
    assert!(inject_agy_headless(&mut e, "x", hour));
    assert_eq!(e.iter().filter(|x| x.contains("output-format")).count(), 1);
    // A prompt that spells a flag the template does not carry is not that flag.
    let mut f = s(&["agy", "-p", "--output-format text"]);
    assert!(inject_agy_headless(&mut f, "--output-format text", hour));
    assert_eq!(f.last().unwrap(), "-p= --output-format text");
    assert!(f
        .windows(2)
        .any(|w| w == ["--output-format", "stream-json"]));
    // No `-p` prompt, or not agy: untouched.
    let mut g = s(&["agy", "--", "x"]);
    assert!(!inject_agy_headless(&mut g, "x", hour));
    assert_eq!(g, s(&["agy", "--", "x"]));
    let mut h = s(&["claude", "-p", "x"]);
    assert!(!inject_agy_headless(&mut h, "x", hour));
    assert_eq!(h, s(&["claude", "-p", "x"]));
    // agy's deadline sits inside the engine's.
    assert_eq!(agy_print_timeout(Duration::from_secs(600)), "540s");
    assert_eq!(agy_print_timeout(Duration::from_millis(500)), "1s");
}

/// Under the floor, agy's permission prompts are off (headless agy soft-denies every shell
/// command); never twice, never for another CLI.
#[test]
fn the_floor_turns_agys_permission_prompts_off_and_no_other_seats() {
    let mut a = s(&["agy", "--output-format", "stream-json", "-p=x"]);
    assert!(crate::worker_sandbox::agy_permissions_under_floor(&mut a));
    assert_eq!(a[1], "--dangerously-skip-permissions");
    assert!(crate::worker_sandbox::agy_permissions_under_floor(&mut a));
    assert_eq!(
        a.iter()
            .filter(|x| *x == "--dangerously-skip-permissions")
            .count(),
        1
    );
    for other in [
        s(&["claude", "-p", "x"]),
        s(&["codex", "exec", "x"]),
        s(&["copilot", "-p", "x"]),
    ] {
        let mut o = other.clone();
        assert!(!crate::worker_sandbox::agy_permissions_under_floor(&mut o));
        assert_eq!(o, other);
    }
}

/// The adapter on real captures: the final answer alone as the unit's text, usage summed over
/// the process's own steps (fresh + cache-read in, thinking inside out), the conversation's
/// cumulative `result.usage` never re-counted on a resumed turn.
#[test]
fn the_agy_adapter_reads_the_answer_and_this_turns_usage() {
    let (text, usage, tools, _) = drive(T1);
    assert_eq!(text, "VERDICT: PASS two\n");
    let u = usage.expect("usage");
    assert_eq!(
        (
            u.input_tokens,
            u.cache_read_tokens,
            u.output_tokens,
            u.cost_usd
        ),
        (100, 40, 8, None)
    );
    assert!(tools.is_empty());
    // Resumed: the result says 120 fresh / 80 cached (both turns); the turn's own step says 60 / 40.
    let (text, usage, ..) = drive(T2);
    assert_eq!(text, "VERDICT: PASS two\n");
    assert!(T2.contains(r#""num_turns":2"#) && T2.contains(r#""input_tokens":120"#));
    let u = usage.expect("usage");
    assert_eq!(
        (u.input_tokens, u.cache_read_tokens, u.output_tokens),
        (100, 40, 8)
    );
    // Streamed in fragments: the result's answer, once.
    let (text, ..) = drive(GW);
    assert_eq!(text, "VERDICT: PASS one\n");
    // A denied tool (no grant): the tool is named, there is no answer text.
    let (text, usage, tools, _) = drive(TOOL_DENIED);
    assert!(text.is_empty(), "{text}");
    assert_eq!(tools, vec!["run_command".to_string()]);
    assert!(usage.is_some());
    // A model error: the status and error are the text, and an all-zero usage is no usage.
    let (text, usage, ..) = drive(ERROR);
    assert!(
        text.starts_with("agy: ERROR: agent executor error") && text.contains("Error 500"),
        "{text}"
    );
    assert!(usage.is_none());
    // Cut off by --print-timeout: no answer, no usage.
    let (text, usage, ..) = drive(PRINT_TIMEOUT);
    assert!(text.is_empty() && usage.is_none(), "{text}");
    // A stream cut before its result keeps what it streamed; a line that is not JSON passes.
    let cut: Vec<&str> = GW
        .lines()
        .filter(|l| !l.contains(r#""event":"result""#))
        .collect();
    let (text, ..) = drive(&cut.join("\n"));
    assert_eq!(text, "VERDICT: PASS one\n");
    let (text, ..) = drive("not json");
    assert_eq!(text, "not json");
}

/// Files the agy tools touch: a write's `TargetFile`, a read's `AbsolutePath` (agy 1.4.2's tool
/// parameters), on DONE tool steps only.
#[test]
fn the_agy_adapter_names_the_files_its_tools_touched() {
    let step = |state: &str, name: &str, params: serde_json::Value| {
        serde_json::json!({"event":"step_update","step_update":{"conversation_id":CONVERSATION,
            "step_index":2,"state":state,"step_type":"tool","tool_name":name,
            "tool_info":{"name":name,"parameters":params}}})
        .to_string()
    };
    let mut a = AgyStreamJson::default();
    let active = a.on_line(&step(
        "ACTIVE",
        "write_to_file",
        serde_json::json!({"TargetFile":"/w/a.rs"}),
    ));
    assert!(active.files.is_empty() && active.tools.is_empty());
    let w = a.on_line(&step(
        "DONE",
        "write_to_file",
        serde_json::json!({"TargetFile":"/w/a.rs","CodeContent":"x"}),
    ));
    assert_eq!((w.files, w.tools), (s(&["/w/a.rs"]), s(&["write_to_file"])));
    let r = a.on_line(&step(
        "DONE",
        "view_file",
        serde_json::json!({"AbsolutePath":"/w/b.rs"}),
    ));
    assert_eq!(r.files, s(&["/w/b.rs"]));
    let c = a.on_line(&step(
        "DONE",
        "run_command",
        serde_json::json!({"CommandLine":"ls"}),
    ));
    assert!(c.files.is_empty());
}

/// The conversation is agy's: read off its `init`, adopted after a completed turn (as codex's
/// thread), resumed with `--conversation <id>` right after the binary, never chosen by the engine
/// and never doubled when the template already names one.
#[test]
fn an_agy_conversation_is_read_from_its_init_and_resumed_next_unit() {
    for capture in [T1, T2] {
        let ids: Vec<String> = capture
            .lines()
            .filter_map(|l| headless_session_id(SeatCli::Agy, l))
            .collect();
        assert_eq!(ids, vec![CONVERSATION.to_string()]);
    }
    assert!(GW
        .lines()
        .any(|l| headless_session_id(SeatCli::Agy, l).is_some()));
    assert_eq!(
        headless_session_id(SeatCli::Agy, T1.lines().last().unwrap()),
        None
    );
    let mut v: serde_json::Value = serde_json::from_str(T1.lines().next().unwrap()).unwrap();
    v["conversation_id"] = serde_json::json!("");
    assert_eq!(headless_session_id(SeatCli::Agy, &v.to_string()), None);
    assert_eq!(headless_session_id(SeatCli::Agy, "not json"), None);
    assert!(
        crate::headless::fresh_id(SeatCli::Agy).is_none(),
        "agy picks its own"
    );
    assert!(crate::headless::valid_session_id(
        SeatCli::Agy,
        CONVERSATION
    ));
    assert!(!crate::headless::valid_session_id(
        SeatCli::Agy,
        "--continue"
    ));
    // Adopted from the stream after a completed turn.
    let (mut tap, seen) =
        crate::headless::SessionTap::new(Box::<AgyStreamJson>::default(), SeatCli::Agy);
    for l in T1.lines() {
        tap.on_line(l);
    }
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.completed, Some(true));
    assert_eq!(
        crate::headless::session_after_turn(SeatCli::Agy, None, &seen).as_deref(),
        Some(CONVERSATION)
    );
    // Resumed right after the binary; a fresh turn gets nothing.
    let mut a = s(&["agy", "--output-format", "stream-json", "-p=next"]);
    assert_eq!(
        crate::headless::inject_session(SeatCli::Agy, &mut a, Some(CONVERSATION), None).as_deref(),
        Some(CONVERSATION)
    );
    assert_eq!(&a[..3], &s(&["agy", "--conversation", CONVERSATION])[..]);
    let mut fresh = s(&["agy", "-p=x"]);
    assert_eq!(
        crate::headless::inject_session(SeatCli::Agy, &mut fresh, None, None),
        None
    );
    assert_eq!(fresh, s(&["agy", "-p=x"]));
    // The template's own session flag wins (`--conversation`, `--continue`, `-c`).
    for own in [
        s(&["agy", "--conversation", "u", "-p=x"]),
        s(&["agy", "--continue", "-p=x"]),
        s(&["agy", "-c", "-p=x"]),
    ] {
        let mut o = own.clone();
        assert_eq!(
            crate::headless::inject_session(SeatCli::Agy, &mut o, Some(CONVERSATION), None),
            None
        );
        assert_eq!(o, own);
    }
}

/// A turn is done only on a `SUCCESS` result: an `ERROR` result, a stream cut before its result,
/// and a `SUCCESS` its own `--print-timeout` cut off (told by agy's stderr) are not.
#[test]
fn an_agy_turn_is_done_only_on_a_success_result_it_was_not_cut_off_from() {
    let lines = |c: &'static str| c.lines().collect::<Vec<_>>();
    assert_eq!(
        headless_turn_completed(SeatCli::Agy, &lines(T1)),
        Some(true)
    );
    assert_eq!(
        headless_turn_completed(SeatCli::Agy, &lines(ERROR)),
        Some(false)
    );
    assert_eq!(
        headless_turn_completed(SeatCli::Agy, &lines(T1)[..2]),
        Some(false)
    );
    assert_eq!(headless_turn_completed(SeatCli::Agy, &[]), Some(false));
    assert_eq!(
        headless_turn_event(SeatCli::Agy, T1.lines().next().unwrap()),
        Some(false)
    );
    assert_eq!(
        headless_turn_event(SeatCli::Agy, T1.lines().last().unwrap()),
        Some(true)
    );
    assert_eq!(
        headless_turn_event(SeatCli::Agy, ERROR.lines().last().unwrap()),
        Some(false)
    );
    assert_eq!(
        headless_turn_event(SeatCli::Agy, T1.lines().nth(1).unwrap()),
        None
    );
    for (capture, done) in [(T1, true), (ERROR, false)] {
        let (mut tap, seen) =
            crate::headless::SessionTap::new(Box::<AgyStreamJson>::default(), SeatCli::Agy);
        for l in capture.lines() {
            tap.on_line(l);
        }
        let (mut code, mut out) = (0, String::new());
        crate::headless::settle_cut_off_turn(&seen.lock().unwrap(), &mut code, &mut out, "agy");
        assert_eq!(code == 0, done, "{out}");
    }
    // The print timeout: the stream says SUCCESS; agy's stderr says otherwise.
    let notice = "[agy] print timeout after 5s with turn in progress; returning partial output\n";
    assert!(agy_print_timed_out(notice));
    assert!(!agy_print_timed_out("agy: some other warning"));
    let (mut code, mut out) = (0, String::from("partial"));
    crate::headless::settle_agy_print_timeout(notice, &mut code, &mut out);
    assert_eq!(code, 1);
    assert!(
        out.contains("--print-timeout") && out.contains("not counted as done"),
        "{out}"
    );
    let (mut code, mut out) = (0, String::new());
    crate::headless::settle_agy_print_timeout("", &mut code, &mut out);
    assert_eq!((code, out.as_str()), (0, ""));
    let (mut code, mut out) = (3, String::new());
    crate::headless::settle_agy_print_timeout(notice, &mut code, &mut out);
    assert_eq!(code, 3, "a failed exit keeps its own code");
}

/// agy's exit codes as its headless docs define them, named on the record; 0 and undocumented
/// codes add nothing, and the runner's exit marker still parses.
#[test]
fn agy_exit_codes_are_named_on_the_record() {
    assert!(agy_exit_meaning(1).unwrap().contains("no response"));
    assert!(agy_exit_meaning(2).unwrap().contains("usage error"));
    assert!(agy_exit_meaning(3).unwrap().contains("partial response"));
    assert_eq!(agy_exit_meaning(0), None);
    assert_eq!(agy_exit_meaning(137), None);
    let line = format!(
        "(cli `agy` exited 3) {}: agy: ERROR: x",
        agy_exit_meaning(3).unwrap()
    );
    assert_eq!(wrapped_exit_code(&line), Some(3));
}

/// codex xr-1049 P2: a cut-off turn is settled in ANY output format (a template's
/// `--output-format text` gets agy's `--print-timeout` too, but no stream to judge), and only
/// agy's, only on exit 0.
#[test]
fn an_agy_print_timeout_is_settled_whatever_the_output_format() {
    let notice = "[agy] print timeout after 9s with turn in progress; returning partial output\n";
    let run = |code: i32, err: &str| -> std::io::Result<BoundedRun> {
        Ok((
            code,
            "partial".into(),
            err.into(),
            None,
            Vec::new(),
            Vec::new(),
        ))
    };
    // Text mode: no headless state at all, still settled.
    let mut text = run(0, notice);
    assert!(settle_agy_cut_off(SeatCli::Agy, &mut text));
    let (code, out, ..) = text.unwrap();
    assert_eq!(code, 1);
    assert!(
        out.starts_with("partial") && out.contains("--print-timeout"),
        "{out}"
    );
    for (cli, code, err) in [
        (SeatCli::Agy, 0, ""),
        (SeatCli::Agy, 3, notice),
        (SeatCli::Claude, 0, notice),
    ] {
        let mut r = run(code, err);
        assert!(!settle_agy_cut_off(cli, &mut r), "{cli:?} {code}");
        assert_eq!(r.unwrap().0, code);
    }
    let mut failed: std::io::Result<BoundedRun> = Err(std::io::ErrorKind::NotFound.into());
    assert!(!settle_agy_cut_off(SeatCli::Agy, &mut failed));
}

/// codex xr-1049 P2: the adapter holds at most [`AGY_HELD_TEXT`] of a turn's text however much
/// it streams, and the runner cuts one oversized delta to the room left (at a UTF-8 boundary).
#[test]
fn the_agy_adapter_and_the_runner_hold_bounded_text() {
    let mut a = AgyStreamJson::default();
    let chunk = "é".repeat(512 * 1024); // 1 MiB of two-byte characters per line
    let line = serde_json::json!({"event":"step_update","step_update":{
        "conversation_id":CONVERSATION,"step_index":1,"state":"ACTIVE",
        "step_type":"agent_response","text_delta":chunk}})
    .to_string();
    for _ in 0..12 {
        a.on_line(&line);
    }
    let held = a.finish().text.join("");
    assert!(held.len() <= AGY_HELD_TEXT, "{}", held.len());
    assert!(held.len() > AGY_HELD_TEXT - 4, "it keeps what fits");
    let mut s = String::from("ab");
    assert!(!push_within(&mut s, "éé", 5));
    assert_eq!(s, "abé", "cut at a character boundary");
    assert!(push_within(&mut s, "", 5));
    assert!(push_within(&mut String::new(), "xyz", 3));
}

/// codex xr-1049 P2, the runner's side: an adapter that hands back one delta bigger than the
/// retained-output cap (a held answer) is cut to the room left, never retained whole.
#[cfg(unix)]
#[test]
fn run_bounded_cuts_one_oversized_delta_to_the_retained_cap() {
    #[derive(Default)]
    struct HoldAll(String);
    impl OutputAdapter for HoldAll {
        fn on_line(&mut self, line: &str) -> AdapterOut {
            self.0.push_str(line);
            AdapterOut::default()
        }
        fn finish(&mut self) -> AdapterOut {
            AdapterOut {
                text: vec![std::mem::take(&mut self.0)],
                ..AdapterOut::default()
            }
        }
    }
    // spawn-audit: test-only — a fixture that prints 10 MiB on one line.
    let mut cmd = Command::new("sh");
    cmd.args(["-c", "head -c 10485760 /dev/zero | tr '\\0' a; echo"]);
    let (code, out, ..) = run_bounded(
        cmd,
        Duration::from_secs(60),
        &|_| {},
        Box::<HoldAll>::default(),
        None,
    )
    .expect("ran");
    assert_eq!(code, 0);
    assert!(out.len() <= 8 * 1024 * 1024 + 64, "{}", out.len());
    assert!(
        out.ends_with("(output truncated)\n"),
        "{}",
        &out[out.len() - 40..]
    );
}
