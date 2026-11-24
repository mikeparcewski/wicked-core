//! (DES-j6-build C0a) A prompt handed on stdin: the argv form ([`stdin_prompt_argv`]), a 150 KB
//! prompt to a tool-less claude and a read-only (evaluator) codex through the real wrapped runner on the host
//! floor, and the same prompt through a seat pod's `kubectl exec -i` client on the container floor.

use super::*;
use wicked_apps_core::spawn::SeatCli;

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

/// A prompt the size of J6 v3's largest writer calls (150 KB), with newlines, quotes, a leading
/// dash and shell metacharacters, so a byte that is re-quoted, split or dropped shows.
fn big_prompt() -> String {
    let mut p = String::from("-not an option\n\"quoted\" 'single' $HOME `tick` \\ back\n");
    while p.len() < 150 * 1024 {
        p.push_str("IF PAY-HOURS > 40 MOVE 'Y' TO WS-OVERTIME. é\n");
    }
    p
}

#[test]
fn a_stdin_prompt_leaves_no_prompt_argument_for_claude_and_a_dash_for_codex() {
    let mut claude = build_argv("claude -p {PROMPT}", STDIN_PROMPT_SLOT, &[]);
    assert_eq!(stdin_prompt_argv(&mut claude, SeatCli::Claude), Ok(None));
    assert_eq!(claude, s(&["claude", "-p"]));
    // An option guard left last goes with the prompt (a variadic flag before it eats nothing).
    let mut guarded = build_argv("claude --add-dir /x {PROMPT}", STDIN_PROMPT_SLOT, &[]);
    assert_eq!(guarded.last().map(String::as_str), Some(STDIN_PROMPT_SLOT));
    stdin_prompt_argv(&mut guarded, SeatCli::Claude).unwrap();
    assert_eq!(guarded, s(&["claude", "--add-dir", "/x"]));
    // A template with no placeholder (an `InputMode::Stdin` seat) gets no prompt argument either.
    let mut bare = build_argv("claude -p --model m", STDIN_PROMPT_SLOT, &[]);
    stdin_prompt_argv(&mut bare, SeatCli::Claude).unwrap();
    assert_eq!(bare, s(&["claude", "-p", "--model", "m"]));
    let mut codex = build_argv(
        "codex exec --skip-git-repo-check {PROMPT}",
        STDIN_PROMPT_SLOT,
        &[],
    );
    assert_eq!(
        stdin_prompt_argv(&mut codex, SeatCli::Codex),
        Ok(Some("-".to_string()))
    );
    assert_eq!(codex, s(&["codex", "exec", "--skip-git-repo-check", "-"]));
    // Past an option guard, `-` is still the positional prompt (stdin).
    let mut guarded = build_argv("codex exec {PROMPT}", STDIN_PROMPT_SLOT, &[]);
    stdin_prompt_argv(&mut guarded, SeatCli::Codex).unwrap();
    assert_eq!(guarded, s(&["codex", "exec", "--", "-"]));
    // Refused: a prompt glued into a token, and agy (its carrier needs argv).
    let mut glued = build_argv("claude --prompt={PROMPT}", STDIN_PROMPT_SLOT, &[]);
    assert!(stdin_prompt_argv(&mut glued, SeatCli::Claude)
        .unwrap_err()
        .contains("own argument"));
    let mut agy = build_argv("agy -p {PROMPT}", STDIN_PROMPT_SLOT, &[]);
    assert!(stdin_prompt_argv(&mut agy, SeatCli::Agy)
        .unwrap_err()
        .contains("agy"));
}

/// What a stand-in seat saw: its argv (one `[arg]` per argument, a long one as
/// `<long crc:len>`) and its stdin (`<stdin crc:len>`, or `<no stdin>` when it is not a pipe),
/// printed as its answer. `len` is the byte length; `crc` is taken with the digits removed, since
/// a read-only codex's prompt names its per-attempt check scratch, whose name holds a clock.
#[cfg(unix)]
const STAND_IN: &str = r#"#!/bin/sh
sum() { tr -d '0-9' | cksum | cut -d' ' -f1; }
out=""
for a in "$@"; do
  n=$(printf '%s' "$a" | wc -c | tr -d ' ')
  if [ "$n" -gt 4096 ]; then a="<long $(printf '%s' "$a" | sum):$n>"; fi
  out="$out[$a]"
done
if [ -p /dev/stdin ]; then
  v=$(cat; echo .); v=${v%.}
  i="<stdin $(printf '%s' "$v" | sum):$(printf '%s' "$v" | wc -c | tr -d ' ')>"
else i="<no stdin>"; fi
printf 'ARGV %s\nSTDIN %s\n' "$out" "$i"
"#;

#[cfg(unix)]
fn line<'a>(out: &'a str, tag: &str) -> &'a str {
    out.lines()
        .find_map(|l| l.strip_prefix(tag))
        .unwrap_or_else(|| panic!("no {tag} line in {out}"))
}

/// Host floor: the same tool-free ballot unit, launched once with its prompt on argv and once on
/// stdin. On stdin the seat reads the bytes argv carried (same length and checksum; claude's
/// prompt is the same bytes, codex's differs only in its scratch's clock digits), and
/// its argv is the argv form of the same launch: claude keeps `--tools ""` and loses only the
/// prompt; codex keeps its evaluator sandbox and gets `-` in the prompt's place.
#[cfg(unix)]
#[test]
fn a_150_kb_prompt_reaches_a_tool_less_claude_and_a_read_only_codex_on_stdin_byte_exact() {
    struct Var(&'static str, Option<std::ffi::OsString>);
    impl Drop for Var {
        fn drop(&mut self) {
            match &self.1 {
                Some(v) => std::env::set_var(self.0, v),
                None => std::env::remove_var(self.0),
            }
        }
    }
    let set = |k: &'static str, v: Option<&std::ffi::OsStr>| {
        let prev = std::env::var_os(k);
        match v {
            Some(v) => std::env::set_var(k, v),
            None => std::env::remove_var(k),
        }
        Var(k, prev)
    };
    let _env = crate::test_env::ENV_LOCK
        .write()
        .unwrap_or_else(|p| p.into_inner());
    let dir = std::env::temp_dir().join(format!(
        "wicked-stdin-prompt-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(dir.join("config")).unwrap();
    let dir = dir.canonicalize().unwrap();
    let _snapshot = set(crate::skills_snapshot::SKILLS_SNAPSHOT_ENV, None);
    let _floor = set(
        crate::container_floor::SEAT_FLOOR_ENV,
        Some(std::ffi::OsStr::new("host")),
    );
    // No live plugin cache: the skills fallback reads an empty config home.
    let _config = set(
        crate::acp_runner::CLAUDE_CONFIG_DIR_ENV,
        Some(dir.join("config").as_os_str()),
    );
    let prompt = big_prompt();
    let run = |cli: &str, on_stdin: bool| {
        let bin = dir.join(cli);
        if !bin.exists() {
            crate::test_env::install_stub(&bin, STAND_IN);
        }
        let mut unit = WorkUnit::pending(COUNCIL_BALLOT_UNIT, "council", 1, prompt.as_str());
        unit.assigned_cli = Some(cli.to_string());
        unit.assigned_invocation = Some(format!(
            "{} {} {{PROMPT}}",
            bin.display(),
            match cli {
                "claude" => "-p",
                _ => "exec --skip-git-repo-check",
            }
        ));
        // Tool-free, as a model call runs: claude answer-only (tool-less), codex a read-only
        // evaluator.
        if cli == "claude" {
            unit.answer_only = true;
        } else {
            unit.role = crate::workflow::PhaseRole::Evaluator;
            unit.worktree_guarded = true;
        }
        let input = StepInput {
            base_commit: None,
            // One run id for both launches, so their sandbox paths match.
            run_id: format!("stdin-{cli}-{}", std::process::id()),
            unit_ix: 0,
            attempt: 0,
            unit,
            workflow_id: "wf-council".to_string(),
            entity_mode: crate::scope::EntityMode::Isolated,
            workdir: None,
            governance: None,
            prior_outputs: vec![],
            elicitation_epoch: 0,
            process_gen: None,
            launch_seq: 0,
            required_skills: Vec::new(),
        };
        let (tx, _rx) = std::sync::mpsc::channel();
        let runner = WrappedCliStepRunner::with_tx(tx);
        let out = if on_stdin {
            with_prompt_on_stdin(|| runner.run_unit(&input))
        } else {
            runner.run_unit(&input)
        };
        let _ = std::fs::remove_dir_all(sandbox_for(&input));
        assert_eq!(
            out.status,
            StepStatus::Ok,
            "{cli} {on_stdin}: {}",
            out.output
        );
        out.output
    };
    for cli in ["claude", "codex"] {
        let on_argv = run(cli, false);
        let on_stdin = run(cli, true);
        let argv_a = line(&on_argv, "ARGV ");
        let argv_b = line(&on_stdin, "ARGV ");
        // The prompt as argv carried it: the one long argument.
        let long = argv_a
            .split(']')
            .find_map(|a| a.strip_prefix("[<long "))
            .unwrap_or_else(|| panic!("{cli}: no long argument in {argv_a}"))
            .trim_end_matches('>')
            .to_string();
        let len: usize = long.rsplit(':').next().unwrap().parse().unwrap();
        assert!(len >= prompt.len(), "{cli}: the argv prompt is {len} bytes");
        assert_eq!(line(&on_argv, "STDIN "), "<no stdin>", "{cli}");
        assert_eq!(
            line(&on_stdin, "STDIN "),
            format!("<stdin {long}>"),
            "{cli}: stdin is not the argv prompt byte for byte"
        );
        assert!(
            !argv_b.contains("<long"),
            "{cli}: a long argument is left: {argv_b}"
        );
        let expected = match cli {
            "claude" => argv_a.replace(&format!("[<long {long}>]"), ""),
            _ => argv_a.replace(&format!("[<long {long}>]"), "[-]"),
        };
        // (Digits aside: codex's check scratch is named with a clock.)
        let digitless = |s: &str| {
            s.chars()
                .filter(|c| !c.is_ascii_digit())
                .collect::<String>()
        };
        assert_eq!(
            digitless(argv_b),
            digitless(&expected),
            "{cli}: the stdin launch's argv"
        );
        if cli == "claude" {
            assert!(argv_b.contains("[--tools][]"), "{argv_b}");
        } else {
            // The read-only evaluator's own codex sandbox: its check scratch, the tree denied.
            assert!(argv_b.contains("[-][--json][--sandbox]"), "{argv_b}");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Container floor: a seat pod's CLI argv rides the unit env file and the CLI runs under
/// `kubectl exec -i`, whose stdin the pod's wrapper (`"$@"`) and launcher (`exec "$@"`) hand to the
/// CLI. A tool-less claude's 150 KB prompt is fed on that client's stdin, byte for byte, and is in
/// neither the unit file nor any kubectl argument (no file pointer, nothing over an exec argument).
#[cfg(unix)]
#[test]
fn a_150_kb_prompt_rides_a_seat_pods_exec_stdin_byte_exact() {
    if crate::validator::skip_when_nested("a_150_kb_prompt_rides_a_seat_pods_exec_stdin_byte_exact")
    {
        return;
    }
    // The wrapper hands its stdin on: nothing in it redirects or closes the CLI's stdin.
    for closes in ["</dev/null", "< /dev/null", "<&-", "0<"] {
        assert!(!crate::k8s::seat::WRAPPER.contains(closes), "{closes}");
    }
    let dir = std::env::temp_dir().join(format!(
        "wicked-k8s-stdin-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    let kubectl = dir.join("kubectl");
    crate::test_env::install_stub(
        &kubectl,
        &format!(
            "#!/bin/sh\nd={d}\necho \"$@\" >> $d/calls\n\
             case \"$*\" in\n\
             *'cat > /wicked-unit/env.tmp'*) cat > $d/env;;\n\
             *'cat /wicked-unit/rc'*) echo 0;;\n\
             *'get sandboxes.agents.x-k8s.io'*) echo '{{\"metadata\":{{\"labels\":{{\"wicked.dev/owner\":\"o\"}}}}}}';;\n\
             *' get pod '*) :;;\n\
             *' delete '*) :;;\n\
             *' exec -i -c seat '*) cat > $d/seat-stdin; echo seat-ran;;\n\
             esac\nexit 0\n",
            d = dir.display()
        ),
    );
    let kube = crate::k8s::Kube {
        kubectl: kubectl.clone(),
        kubeconfig: None,
        context: "t".into(),
        owner: "o".into(),
    };
    let _seat = crate::k8s::seat::register_for_test(&kube, "stdinseat1", &dir, "claude");
    // The seat CLI's argv as the runner builds it for a tool-less claude with its prompt on stdin.
    let mut cli = build_argv("claude -p {PROMPT}", STDIN_PROMPT_SLOT, &[]);
    assert_eq!(stdin_prompt_argv(&mut cli, SeatCli::Claude), Ok(None));
    apply_tool_less_posture(&mut cli, Vec::new(), true, true).unwrap();
    inject_claude_stream_flags(&mut cli);
    let mut wrapper = kube.head(Some("ns"));
    wrapper.extend(
        [
            "exec",
            "-i",
            "-c",
            "seat",
            "seat-1",
            "--",
            "/floor/busybox",
            "sh",
            "-c",
            crate::k8s::seat::WRAPPER,
            "wicked-unit-stdinseat1",
            "/floor/launch",
            "/wicked-canary/probe",
            "--",
            "/floor/landrun",
            "--rox",
            "/usr",
            "--",
            "/floor/launch",
            "--check",
            "/wicked-canary/probe",
            "--",
        ]
        .map(str::to_string),
    );
    // spawn-audit: test-only — a fake kubectl script; proves the pod seat's stdin wiring.
    let mut cmd = Command::new(&wrapper[0]);
    cmd.args(&wrapper[1..]).args(&cli).current_dir(&dir);
    let prompt = big_prompt();
    let emit = |_: &str| {};
    let (code, out, ..) = run_bounded_fed(
        cmd,
        Some(prompt.clone().into_bytes()),
        crate::test_env::PATIENT,
        &emit,
        Box::new(Passthrough),
        None,
    )
    .expect("ran");
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("seat-ran"), "{out}");
    let fed = std::fs::read(dir.join("seat-stdin")).unwrap();
    assert!(
        fed == prompt.as_bytes(),
        "the pod's exec stdin is not the prompt ({} bytes)",
        fed.len()
    );
    let env = std::fs::read_to_string(dir.join("env")).unwrap();
    assert!(
        env.contains("'claude' '-p'") && env.contains("'--tools' ''"),
        "{env}"
    );
    let calls = std::fs::read_to_string(dir.join("calls")).unwrap();
    for text in [&env, &calls] {
        assert!(
            !text.contains("PAY-HOURS"),
            "the prompt rode a file or an argument"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
