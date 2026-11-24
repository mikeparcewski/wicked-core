//! (DES-j6-build C0a) A prompt handed on stdin: the argv form ([`stdin_prompt_argv`]); a 150 KB
//! prompt to a tool-less claude and a read-only (evaluator) codex through the real wrapped runner
//! on the host floor; a feed the seat does not read whole; and the same prompt through a seat
//! pod's `kubectl exec -i` client on the container floor.

use super::*;
use wicked_apps_core::spawn::SeatCli;

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

/// A prompt of `size` bytes or a little more, with newlines, quotes, a leading dash, shell
/// metacharacters, non-ASCII and leading and trailing whitespace, so a byte that is re-quoted,
/// split, trimmed or dropped shows.
fn prompt_of(size: usize) -> String {
    let mut p = String::from("  -not an option\n\"quoted\" 'single' $HOME `tick` \\ back\n");
    while p.len() < size {
        p.push_str("IF PAY-HOURS > 40 MOVE 'Y' TO WS-OVERTIME. é\n");
    }
    p.push_str("\n\n");
    p
}

/// J6 v3's largest writer calls are about 150 KB.
fn big_prompt() -> String {
    prompt_of(150 * 1024)
}

#[test]
fn a_stdin_prompt_leaves_no_prompt_argument_for_claude_and_a_dash_for_codex() {
    let conv = |template: &str, seat| {
        let mut argv = build_argv(template, STDIN_PROMPT_SLOT, &[]);
        stdin_prompt_argv(&mut argv, seat).map(|p| (argv, p))
    };
    assert_eq!(
        conv("claude -p {PROMPT}", SeatCli::Claude),
        Ok((s(&["claude", "-p"]), None))
    );
    // An option guard left last goes with the prompt (a variadic flag before it eats nothing).
    assert_eq!(
        conv("claude --add-dir /x {PROMPT}", SeatCli::Claude),
        Ok((s(&["claude", "--add-dir", "/x"]), None))
    );
    // A template with no placeholder (an `InputMode::Stdin` seat) gets no prompt argument either.
    assert_eq!(
        conv("claude -p --model m", SeatCli::Claude),
        Ok((s(&["claude", "-p", "--model", "m"]), None))
    );
    let dash = Some("-".to_string());
    assert_eq!(
        conv("codex exec --skip-git-repo-check {PROMPT}", SeatCli::Codex),
        Ok((
            s(&["codex", "exec", "--skip-git-repo-check", "-"]),
            dash.clone()
        ))
    );
    // Past an option guard, `-` is still the positional prompt (stdin).
    assert_eq!(
        conv("codex exec {PROMPT}", SeatCli::Codex),
        Ok((s(&["codex", "exec", "--", "-"]), dash.clone()))
    );
    // A codex template that already reads stdin keeps its one `-`.
    assert_eq!(
        conv("codex exec -", SeatCli::Codex),
        Ok((s(&["codex", "exec", "-"]), dash))
    );
    // Refused: a prompt joined to other text, a prompt placed twice, and every other seat.
    let refused = |template: &str, seat| conv(template, seat).unwrap_err();
    assert!(refused("claude --prompt={PROMPT}", SeatCli::Claude).contains("own argument"));
    assert!(refused("claude -p {PROMPT} {PROMPT}", SeatCli::Claude).contains("2 times"));
    assert!(refused("codex exec {PROMPT} {PROMPT}", SeatCli::Codex).contains("2 times"));
    for (template, seat) in [
        ("agy -p {PROMPT}", SeatCli::Agy),
        ("pi -p {PROMPT}", SeatCli::Pi),
        ("copilot -p {PROMPT}", SeatCli::Copilot),
    ] {
        assert!(
            refused(template, seat).contains("only claude and codex"),
            "{template}"
        );
    }
}

/// A stand-in seat: it writes each argument over 4 KB to `<dir>/<cli>.argv-prompt` (printing
/// `[<long>]` in its place) and its stdin, when that is a pipe, to `<dir>/<cli>.stdin`, then prints
/// its argv as `ARGV [a][b]…` and `STDIN pipe|none`.
#[cfg(unix)]
fn stand_in(dir: &Path, cli: &str) -> String {
    format!(
        r#"#!/bin/sh
d='{d}'
out=""
for a in "$@"; do
  n=$(printf '%s' "$a" | wc -c | tr -d ' ')
  if [ "$n" -gt 4096 ]; then printf '%s' "$a" > "$d/{cli}.argv-prompt"; a="<long>"; fi
  out="$out[$a]"
done
if [ -p /dev/stdin ]; then cat > "$d/{cli}.stdin"; i=pipe; else i=none; fi
printf 'ARGV %s\nSTDIN %s\n' "$out" "$i"
"#,
        d = dir.display()
    )
}

#[cfg(unix)]
fn line<'a>(out: &'a str, tag: &str) -> &'a str {
    out.lines()
        .find_map(|l| l.strip_prefix(tag))
        .unwrap_or_else(|| panic!("no {tag} line in {out}"))
}

/// `b` with every run of 13 or more ASCII digits as `#`: a read-only codex's check scratch is
/// named with a nanosecond clock, the only part of a launch that differs between two launches of
/// the same unit. Nothing else is normalized.
#[cfg(unix)]
fn unclocked(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let run = b[i..].iter().take_while(|c| c.is_ascii_digit()).count();
        if run >= 13 {
            out.push(b'#');
            i += run;
        } else if run > 0 {
            out.extend_from_slice(&b[i..i + run]);
            i += run;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

/// Host floor, through the real wrapped runner with stand-in `claude` and `codex` binaries, tool
/// free as a model call runs (claude answer-only, codex a read-only evaluator):
/// - the same unit with an 8 KB prompt, launched with it on argv and then on stdin: the seat
///   reads on stdin exactly the bytes argv carried, and its argv is the argv launch's with only
///   the prompt gone (claude, keeping `--tools ""`) or `-` in its place (codex, keeping its
///   sandbox);
/// - a 150 KB prompt (over Linux's 128 KiB argument limit) on stdin: the seat reads exactly the
///   launch's prompt, the 8 KB launch's bytes with the 150 KB task in place of the 8 KB one.
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
    // One launch: (its printed answer, the long argv argument, its stdin), both read back and
    // removed.
    let run = |cli: &str, task: &str, on_stdin: bool| {
        let bin = dir.join(cli);
        if !bin.exists() {
            crate::test_env::install_stub(&bin, &stand_in(&dir, cli));
        }
        let mut unit = WorkUnit::pending(COUNCIL_BALLOT_UNIT, "council", 1, task);
        unit.assigned_cli = Some(cli.to_string());
        let flags = if cli == "claude" {
            "-p"
        } else {
            "exec --skip-git-repo-check"
        };
        unit.assigned_invocation = Some(format!("{} {flags} {{PROMPT}}", bin.display()));
        if cli == "claude" {
            unit.answer_only = true;
        } else {
            unit.role = crate::workflow::PhaseRole::Evaluator;
            unit.worktree_guarded = true;
        }
        let input = StepInput {
            base_commit: None,
            // One run id for every launch, so their sandbox paths match.
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
        let take = |ext: &str| {
            let p = dir.join(format!("{cli}.{ext}"));
            let b = std::fs::read(&p).ok();
            let _ = std::fs::remove_file(&p);
            b
        };
        (out.output, take("argv-prompt"), take("stdin"))
    };
    let small = prompt_of(8 * 1024);
    let big = big_prompt();
    for cli in ["claude", "codex"] {
        let (out_a, argv_prompt, none) = run(cli, &small, false);
        let (out_b, no_arg, fed) = run(cli, &small, true);
        let argv_prompt = argv_prompt.unwrap_or_else(|| panic!("{cli}: no prompt argument"));
        assert!(none.is_none(), "{cli}: an argv launch was fed stdin");
        assert_eq!(line(&out_a, "STDIN "), "none", "{cli}");
        assert!(
            no_arg.is_none(),
            "{cli}: a stdin launch kept a long argument"
        );
        assert_eq!(line(&out_b, "STDIN "), "pipe", "{cli}");
        let fed = fed.unwrap_or_else(|| panic!("{cli}: nothing on stdin"));
        assert!(
            unclocked(&fed) == unclocked(&argv_prompt),
            "{cli}: stdin is not the argv prompt byte for byte"
        );
        if cli == "claude" {
            assert!(
                fed == argv_prompt,
                "claude: stdin differs from its argv prompt"
            );
        }
        let (argv_a, argv_b) = (line(&out_a, "ARGV "), line(&out_b, "ARGV "));
        let expected = match cli {
            "claude" => argv_a.replace("[<long>]", ""),
            _ => argv_a.replace("[<long>]", "[-]"),
        };
        assert_eq!(
            unclocked(argv_b.as_bytes()),
            unclocked(expected.as_bytes()),
            "{cli}: the stdin launch's argv"
        );
        if cli == "claude" {
            assert!(argv_b.contains("[--tools][]"), "{argv_b}");
        } else {
            // The read-only evaluator's own codex sandbox (its check scratch, the tree denied).
            assert!(argv_b.contains("[-][--json][--sandbox]"), "{argv_b}");
        }
        // 150 KB on stdin: the same launch with the big task in place of the small one.
        let (_, no_arg, fed) = run(cli, &big, true);
        assert!(no_arg.is_none(), "{cli}: the 150 KB prompt rode argv");
        let fed = fed.unwrap_or_else(|| panic!("{cli}: nothing on stdin"));
        let small_at = argv_prompt
            .windows(small.len())
            .position(|w| w == small.as_bytes())
            .unwrap_or_else(|| panic!("{cli}: the task is not in its prompt"));
        let mut want = argv_prompt[..small_at].to_vec();
        want.extend_from_slice(big.as_bytes());
        want.extend_from_slice(&argv_prompt[small_at + small.len()..]);
        assert!(fed.len() > 150 * 1024, "{cli}: {} bytes", fed.len());
        assert!(
            unclocked(&fed) == unclocked(&want),
            "{cli}: the 150 KB stdin is not the launch's prompt byte for byte"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A seat that exits 0 without reading its whole prompt has not answered it: the launch reports
/// the short feed and fails. A descendant that holds stdin open without reading (in a session of
/// its own, so the group kill misses it) does not keep the feed, or the launch, alive.
#[cfg(unix)]
#[test]
fn a_seat_that_does_not_read_its_whole_prompt_fails_and_the_feed_ends_with_it() {
    let emit = |_: &str| {};
    let feed = |script: &str| {
        // spawn-audit: test-only — a shell that reads part of its stdin.
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", script]);
        let start = Instant::now();
        let r = run_bounded_fed(
            cmd,
            Some(big_prompt().into_bytes()),
            crate::test_env::PATIENT,
            &emit,
            Box::new(Passthrough),
            None,
        )
        .expect("ran");
        (r.0, r.2, start.elapsed())
    };
    let (code, err, _) = feed("head -c 100 >/dev/null; echo answered; exit 0");
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("before it read its whole prompt"), "{err}");
    let (code, err, took) = feed(
        "python3 -c 'import os,sys,time\nif os.fork():\n    sys.exit(0)\nos.setsid()\n\
         n=os.open(os.devnull,os.O_WRONLY)\nos.dup2(n,1)\nos.dup2(n,2)\ntime.sleep(30)'",
    );
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("before it read its whole prompt"), "{err}");
    assert!(
        took < Duration::from_secs(20),
        "the feed outlived its seat: {took:?}"
    );
    // A seat that reads it all answers as usual.
    let (code, err, _) = feed("cat >/dev/null; echo answered");
    assert_eq!(code, 0, "{err}");
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
