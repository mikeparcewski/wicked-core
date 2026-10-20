//! agy's API-key path: the pasted Gemini API key (never echoed, never repeated in a refusal), its
//! broker kind and status, its gateway route (the unit key in `x-goog-api-key` swapped for the
//! broker's), the seat's env, and LIVE the whole journey: sign-in, ingest, a governed agy seat
//! answering through the route against a fake Gemini API, sign-out. FAKE keys only.

use super::*;
use crate::seat_credentials::CredentialKind;
use crate::seat_gateway::{GEMINI_KEY_FILE, GEMINI_KEY_HEADER, GEMINI_PORT, GEMINI_UPSTREAM};

/// A FAKE key in the shape AI Studio issues (`AIza` + 35 characters).
const FAKE_KEY: &str = "AIzaFAKE-agy-seat-key-000000000000000";

/// The paste becomes the key; every refusal names its reason and repeats nothing of the paste.
#[test]
fn a_pasted_gemini_api_key_is_validated_and_never_repeated() {
    assert_eq!(gemini_api_key(FAKE_KEY.as_bytes()).unwrap(), FAKE_KEY);
    assert_eq!(
        gemini_api_key(format!("  {FAKE_KEY}\r\n").as_bytes()).unwrap(),
        FAKE_KEY
    );
    // A bracketed paste (a terminal that has the mode on) is the key inside it.
    let bracketed = format!("\u{1b}[200~{FAKE_KEY}\u{1b}[201~");
    assert_eq!(gemini_api_key(bracketed.as_bytes()).unwrap(), FAKE_KEY);
    let secretish = "AIzaFAKE-SECRET-PART";
    for (paste, why) in [
        (String::new(), "no key was pasted"),
        ("   \n".to_string(), "no key was pasted"),
        (format!("{secretish} 1234567890"), "space or a line break"),
        (format!("{secretish}\n{secretish}"), "space or a line break"),
        (
            format!("{secretish}!@#$%^&*()"),
            "characters a Gemini API key does not",
        ),
        ("AIzaSHORT".to_string(), "shorter than a key"),
        ("A".repeat(300), "longer than a key"),
        (format!("\u{1b}[200~{secretish}"), "did not finish"),
    ] {
        let err = gemini_api_key(paste.as_bytes()).unwrap_err();
        assert!(err.contains(why), "{paste:?}: {err}");
        assert!(
            !err.contains("FAKE-SECRET"),
            "the refusal repeats the paste: {err}"
        );
    }
    assert!(gemini_api_key(&[0xff, 0xfe])
        .unwrap_err()
        .contains("not text"));
}

/// Ingest moves the paste into the broker file the route serves, and only that; the broker reads
/// back as an API-key sign-in with no expiry, and as signed out when the file is empty.
#[test]
fn an_agy_sign_in_is_its_gemini_api_key_in_the_broker() {
    assert_eq!(broker_files("agy").unwrap(), &[GEMINI_KEY_FILE]);
    assert_eq!(credential_paths("agy"), &[AGY_API_KEY_PATH]);
    let tokens = extract_tokens(
        "agy",
        &[(
            AGY_API_KEY_PATH.to_string(),
            format!("{FAKE_KEY}\n").into_bytes(),
        )],
    )
    .unwrap();
    assert_eq!(
        tokens,
        vec![(GEMINI_KEY_FILE.to_string(), FAKE_KEY.as_bytes().to_vec())]
    );
    let none = extract_tokens("agy", &[]).unwrap_err();
    assert!(
        none.contains("signed out") && none.contains("Gemini API key"),
        "{none}"
    );
    // Files a foreign login left are never credentials.
    assert!(extract_tokens(
        "agy",
        &[("agy/oauth_creds.json".to_string(), b"{}".to_vec())]
    )
    .is_err());
    let held = vec![(GEMINI_KEY_FILE.to_string(), FAKE_KEY.as_bytes().to_vec())];
    assert_eq!(
        status_from_files("agy", &held, 1),
        BrokerStatus::SignedIn { expires_at: None }
    );
    assert_eq!(
        status_from_files("agy", &[(GEMINI_KEY_FILE.to_string(), Vec::new())], 1),
        BrokerStatus::SignedOut
    );
    assert_eq!(
        kind_from_files("agy", &held),
        Some(CredentialKind::GeminiApiKey)
    );
    assert_eq!(kind_from_files("agy", &[]), None);
    let sign_in = sign_in_from_files("agy", &held, 1).unwrap();
    assert_eq!(sign_in.kind, Some(CredentialKind::GeminiApiKey));
    assert!(sign_in.subscriptions.is_empty());
    assert!(supported_cli("agy") && !host_login("agy"));
    // An API key is never renewed.
    assert!(!crate::seat_refresh::REFRESHABLE.contains(&"agy"));
}

/// The login container runs only the paste script: no network, the operator's uid never, the key
/// saved 0600 under the throwaway home. The script itself (run here without a terminal) saves
/// exactly the key and prints none of it.
#[test]
fn agys_login_reads_the_key_unechoed_into_the_throwaway_volume() {
    let argv = login_argv(
        Path::new("/usr/bin/docker"),
        "agy",
        "wicked-seat-agy:t",
        "vol",
        "none",
    );
    let has = |pair: &[&str]| argv.windows(2).any(|w| w == pair);
    assert!(has(&["--network", "none"]), "{argv:?}");
    assert!(has(&["-e", "WICKED_LOGIN_DIR=/home/login/agy"]));
    assert!(has(&["-v", "vol:/home/login"]));
    let at = argv.iter().position(|a| a == "wicked-seat-agy:t").unwrap();
    assert_eq!(&argv[at + 1..at + 3], ["sh", "-c"]);
    assert!(argv[at + 3].contains("stty -echo"));
    assert!(argv[at + 3].contains("(it is not shown)"));
    assert!(
        !argv.iter().any(|a| a == "agy"),
        "agy itself never runs at sign-in"
    );
    let dir = std::env::temp_dir().join(format!(
        "wicked-agy-login-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let run = |stdin: &str| {
        use std::io::Write;
        // spawn-audit: test-only — runs the login script itself, with a FAKE key on stdin.
        let mut c = std::process::Command::new("sh");
        c.args(["-c", AGY_API_KEY_LOGIN, "wicked-login"])
            .env("WICKED_LOGIN_DIR", dir.join("agy"))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = c.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    let out = run(&format!("{FAKE_KEY}\n"));
    let shown = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{shown}");
    assert!(
        !shown.contains("FAKE-agy-seat"),
        "the key is never printed: {shown}"
    );
    assert!(shown.contains("key received"), "{shown}");
    let saved = dir.join("agy/wicked-gemini-key");
    assert_eq!(std::fs::read_to_string(&saved).unwrap(), FAKE_KEY);
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&saved).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    std::fs::remove_file(&saved).unwrap();
    let empty = run("\n");
    assert!(!empty.status.success());
    assert!(String::from_utf8_lossy(&empty.stderr).contains("no key was pasted"));
    assert!(!saved.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

/// The gateway's Gemini route (pure render): on its own port, it admits only agy's unit keys,
/// read from `x-goog-api-key`; it sends the broker's key in that same header to the Gemini API
/// itself over TLS, passing the native wire through. The route is a default one, owned by agy.
#[test]
fn the_gemini_route_swaps_the_unit_key_for_the_brokers() {
    use crate::seat_gateway::{render, route_owner, GatewayConfig, UnitKey};
    assert_eq!(route_owner(GEMINI_KEY_FILE), Some("agy"));
    let mut config = GatewayConfig::default();
    let route = config
        .providers
        .iter()
        .find(|r| r.port == GEMINI_PORT)
        .expect("a default route")
        .clone();
    assert_eq!(route.kind, Some(CredentialKind::GeminiApiKey));
    assert_eq!(route.owner(), Some("agy"));
    let agy = UnitKey::new("run", "run#1", "agy");
    let claude = UnitKey::new("run", "run#2", "claude");
    config.units = vec![agy.clone(), claude.clone()];
    let rendered = render(&config);
    let bind = rendered["binds"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["port"] == GEMINI_PORT)
        .expect("the Gemini listener");
    let r = &bind["listeners"][0]["routes"][0];
    let p = &r["policies"];
    assert_eq!(p["apiKey"]["mode"], "strict");
    assert_eq!(
        p["apiKey"]["location"],
        serde_json::json!({"header": {"name": GEMINI_KEY_HEADER}})
    );
    let keys: Vec<&str> = p["apiKey"]["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, vec![agy.key.as_str()], "only agy's units");
    assert_eq!(
        p["backendAuth"],
        serde_json::json!({"key": {
            "value": {"file": format!("/broker/{GEMINI_KEY_FILE}")},
            "location": {"header": {"name": GEMINI_KEY_HEADER}}
        }})
    );
    assert_eq!(p["backendTLS"], serde_json::json!({}));
    assert_eq!(p["ai"]["routes"], serde_json::json!({"*": "passthrough"}));
    assert_eq!(
        r["backends"],
        serde_json::json!([{"host": GEMINI_UPSTREAM}])
    );
    // No other model route admits agy's unit.
    for b in rendered["binds"].as_array().unwrap() {
        let model_route = config.providers.iter().any(|r| b["port"] == r.port);
        if b["port"] == GEMINI_PORT || !model_route {
            continue;
        }
        let keys = b.pointer("/listeners/0/routes/0/policies/apiKey/keys");
        assert!(
            !keys.is_some_and(|k| k.to_string().contains(&agy.key)),
            "{}",
            b["port"]
        );
    }
}

/// An agy seat on its API key reaches the route and nothing else: its key is the unit's, its
/// base URL the route's; no other provider's variable reaches it; the CONNECT proxy stays.
#[test]
fn an_agy_seat_on_its_api_key_gets_the_unit_key_and_the_route() {
    let env = crate::seat_gateway::seat_env_for_sign_in(
        "agy",
        &CredentialKind::GeminiApiKey.into(),
        "wk-unit",
    );
    let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    assert_eq!(get("GEMINI_API_KEY"), Some("wk-unit"));
    assert_eq!(
        get("GOOGLE_GEMINI_BASE_URL"),
        Some(format!("http://agw:{GEMINI_PORT}").as_str())
    );
    assert_eq!(get("HTTPS_PROXY"), Some("http://agw:3000"));
    for foreign in [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_BASE_URL",
        "OPENAI_API_KEY",
        "OPENAI_BASE_URL",
    ] {
        assert_eq!(get(foreign), None, "{foreign}");
    }
    assert_eq!(
        crate::seat_gateway::credential_gateway_url("agy", CredentialKind::GeminiApiKey),
        format!("http://agw:{GEMINI_PORT}")
    );
    assert!(crate::container_floor::GATEWAY_PORTS.contains(&GEMINI_PORT));
    // No placeholder credential file: the key rides the env alone.
    assert!(crate::seat_credentials::seat_files(
        "agy",
        &CredentialKind::GeminiApiKey.into(),
        "wk-unit",
        "http://agw:4013"
    )
    .is_empty());
}

/// The fake Gemini API (`tests/fixtures/agy`), test-only.
const FAKE_API: &str = include_str!("../tests/fixtures/agy/fake-gemini-api.js");

/// LIVE (`WICKED_CONTAINER_LIVE=1` + a runtime; skipped otherwise): agy's API-key path end to end
/// with FAKE keys, through the real gateway, the real agy 1.4.2 seat and the real floor.
/// 1. signed out: the governed seat is refused (`agent_signed_out`);
/// 2. the login argv crew runs for Agents -> Sign in, in the engine's PTY: the key is pasted at a
///    prompt that does not echo it, and the transcript never shows it;
/// 3. ingest moves it into the broker (`agy.gemini.key`) and removes the login volume;
/// 4. the armed seat holds only its unit key; the floor wrote its settings read-only; agy's
///    requests reach the (fake) Gemini API through the route, which sees the broker's key and
///    never the unit's; the turn answers; agy's other hosts are refused at the gateway and the
///    turn does not wait on them;
/// 5. another agent's unit key is refused on the route;
/// 6. after sign-out the seat is refused again.
#[test]
fn live_agys_api_key_reaches_the_gemini_api_only_through_its_route() {
    use crate::container_floor::{arm, ensure_seat_image, output_within};
    use crate::seat_gateway::{
        attach, configure_gateway, ensure_gateway, register_live, GatewayConfig, UnitKey,
    };
    use crate::worker_sandbox::{FloorInputs, FloorUnarmed};
    let _env = crate::test_env::ENV_LOCK
        .write()
        .unwrap_or_else(|p| p.into_inner());
    if std::env::var("WICKED_CONTAINER_LIVE").ok().as_deref() != Some("1") {
        eprintln!("skipped: set WICKED_CONTAINER_LIVE=1 with a container runtime to run");
        return;
    }
    struct Restore(Vec<(&'static str, Option<std::ffi::OsString>)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            for (k, v) in self.0.drain(..) {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }
    let _restore = Restore(
        [
            "WICKED_SEAT_IMAGE_AGY",
            "WICKED_WORKER_HOME",
            "WICKED_SEAT_NETWORK",
        ]
        .into_iter()
        .map(|k| (k, std::env::var_os(k)))
        .collect(),
    );
    std::env::remove_var("WICKED_SEAT_IMAGE_AGY");
    std::env::remove_var("WICKED_SEAT_NETWORK");
    let rt = crate::container_floor::runtime().expect("a container runtime answers");
    let image = ensure_seat_image(&rt, "agy").expect("the agy seat image");
    let base = std::env::temp_dir().canonicalize().unwrap().join(format!(
        "wicked-agy-key-live-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let dir = base.join("state/gateway");
    let worker = base.join("worker");
    let home = worker.join("agy-seat");
    let gemini = home.join(".gemini");
    let api = base.join("api");
    for d in [
        &dir,
        &gemini.join("antigravity-cli"),
        &gemini.join("config"),
        &api,
    ] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(api.join("fake-gemini-api.js"), FAKE_API).unwrap();
    let log = api.join("fake.log");
    std::env::set_var("WICKED_WORKER_HOME", &worker);
    let run = |argv: &[&str]| {
        let mut c = rt.cmd();
        c.args(argv);
        output_within(c, Duration::from_secs(60)).unwrap()
    };
    let gw = Gateway::for_dir(&dir);
    let stub = format!("{}-stub", gw.name);
    let mut config = GatewayConfig::default();
    for route in &mut config.providers {
        route.upstream_host = format!("{stub}:8080");
        route.tls = false;
    }
    configure_gateway(&dir, &config).unwrap();
    let gw = ensure_gateway(&rt, &dir).expect("the gateway starts");
    // Everything this test starts goes, however it ends.
    struct Cleanup<'a>(&'a Runtime, Vec<String>, Vec<String>, std::path::PathBuf);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            for c in &self.1 {
                let mut x = self.0.cmd();
                x.args(["rm", "-f", c]);
                let _ = output_within(x, Duration::from_secs(30));
            }
            for v in &self.2 {
                let mut x = self.0.cmd();
                x.args(["volume", "rm", "-f", v]);
                let _ = output_within(x, Duration::from_secs(30));
            }
            let _ = std::fs::remove_dir_all(&self.3);
        }
    }
    let (cfg, broker) = crate::seat_gateway::volumes(&gw);
    let mut cleanup = Cleanup(
        &rt,
        vec![stub.clone(), gw.name.clone()],
        vec![cfg, broker],
        base.clone(),
    );
    let user = {
        // SAFETY: getuid/getgid have no preconditions.
        let (u, g) = unsafe { (libc::getuid(), libc::getgid()) };
        format!("{u}:{g}")
    };
    let api_mount = format!("type=bind,src={a},dst={a}", a = api.display());
    let srv_log = format!("SRV_LOG={}", log.display());
    let api_js = api
        .join("fake-gemini-api.js")
        .to_string_lossy()
        .into_owned();
    let o = run(&[
        "run",
        "-d",
        "--name",
        &stub,
        "--network",
        &gw.external_network,
        "--user",
        &user,
        "--mount",
        &api_mount,
        "--env",
        "PORT=8080",
        "--env",
        "HOST=0.0.0.0",
        "--env",
        &srv_log,
        "--entrypoint",
        "node",
        &image,
        &api_js,
    ]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    // A real linked run worktree with the engine's pin, as a governed unit has.
    let repo = base.join("repo");
    let wt = base.join("wt");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |cwd: &Path, args: &[&str]| {
        // spawn-audit: test-only — local git over a temp repo.
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
    };
    git(&repo, &["init", "-q"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "i"]);
    git(&repo, &["worktree", "add", "-q", wt.to_str().unwrap()]);
    let pin = crate::worktree_guard::pinned_git_dir(&wt, &repo).unwrap();
    let inputs = FloorInputs {
        cwd: wt.clone(),
        write_roots: vec![wt.clone()],
        read_only: false,
        seat_dirs: vec![
            home.clone(),
            gemini.join("antigravity-cli"),
            gemini.join("config"),
            gemini.clone(),
        ],
        unmask: vec![],
        pinned: vec![],
        net: vec![],
        home: None,
        trusted_git_dir: Some(pin),
    };
    let prompt = "AGY-ECHO=keyroute";
    let minute = Duration::from_secs(60);
    let mut argv = crate::execute_wrapped::build_argv("agy -p \"{PROMPT}\"", prompt, &[]);
    assert!(crate::execute_wrapped::inject_agy_headless(
        &mut argv, prompt, minute
    ));
    let seat = |ord: u32| {
        arm(
            &inputs,
            &argv,
            "run-agy-key",
            ord,
            &[],
            None,
            Some(prompt),
            Some(&dir),
        )
    };
    // 1. Signed out.
    let before = seat(1).map(|_| ()).unwrap_err();
    // 2. Agents -> Sign in, in the engine's PTY.
    let login = prepare_login_argv(&rt, &gw, &dir, "agy", &image).expect("the login argv");
    let volume = read_session(&dir, &gw, "agy")
        .unwrap()
        .expect("a login session");
    cleanup.2.push(volume.clone());
    let (transcript, exit) = super::tests::login_terminal_pasting(&login, None, Some(FAKE_KEY));
    // 3. Ingest, as crew runs it on the terminal's exit.
    let report = ingest_and_route(&rt, &gw, &dir, "agy").expect("ingested");
    let login_gone = !volume_exists(&rt, &volume).unwrap();
    // 4. The governed seat.
    let mut armed = seat(2).expect("a signed-in agy seat arms");
    let get = |k: &str| {
        armed
            .env
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.clone())
    };
    let unit_key = get("GEMINI_API_KEY").expect("the unit key");
    let base_url = get("GOOGLE_GEMINI_BASE_URL");
    crate::worker_sandbox::agy_permissions_under_floor(&mut armed.argv);
    let full: Vec<String> = armed.wrapper.iter().chain(&armed.argv).cloned().collect();
    let mut cmd = crate::execute_wrapped::build_worker_command(&full, None);
    cmd.current_dir(&wt);
    cmd.env("HOME", &home);
    for (k, v) in &armed.env {
        cmd.env(k, v);
    }
    let (tap, seen) = crate::headless::SessionTap::new(
        Box::<crate::execute_wrapped::AgyStreamJson>::default(),
        wicked_apps_core::spawn::SeatCli::Agy,
    );
    let started = std::time::Instant::now();
    let turn = crate::execute_wrapped::run_bounded(
        cmd,
        Duration::from_secs(120),
        &|_| {},
        Box::new(tap),
        None,
    )
    .expect("the seat ran");
    let took = started.elapsed();
    let seen = seen.lock().unwrap().clone();
    let settings =
        std::fs::read_to_string(gemini.join("antigravity-cli/settings.json")).unwrap_or_default();
    // 5. Another agent's unit on the route.
    let seat_net = format!("{}-seat", gw.name);
    run(&["network", "rm", &seat_net]);
    assert!(run(&["network", "create", "--internal", &seat_net])
        .status
        .success());
    attach(&rt, &gw, &seat_net).expect("attached as agw");
    let claude = UnitKey::new("run-agy-key", "run-agy-key#9", "claude");
    register_live(&rt, &gw, &dir, claude.clone()).unwrap();
    let curl = |key: &str| {
        let hdr = format!("{GEMINI_KEY_HEADER}: {key}");
        let url = format!("http://agw:{GEMINI_PORT}/v1beta/models/m:generateContent");
        let o = run(&[
            "run",
            "--rm",
            "--network",
            &seat_net,
            "--entrypoint",
            "curl",
            &image,
            "-s",
            "--max-time",
            "5",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "-H",
            &hdr,
            "-d",
            "{}",
            &url,
        ]);
        String::from_utf8_lossy(&o.stdout).to_string()
    };
    let foreign = curl(&claude.key);
    let unknown = curl("wk-FAKE-unknown");
    if let Some(lease) = armed.lease.take() {
        let _ = lease.release();
    }
    drop(armed.cleanup);
    // 6. Sign out.
    sign_out_and_route(&rt, &gw, &dir, "agy").expect("signed out");
    let after = seat(3).map(|_| ()).unwrap_err();
    let logs = run(&["logs", &gw.name]);
    let gw_logs = format!(
        "{}{}",
        String::from_utf8_lossy(&logs.stdout),
        String::from_utf8_lossy(&logs.stderr)
    );
    let upstream = std::fs::read_to_string(&log).unwrap_or_default();
    // agy's other hosts, as the gateway's CONNECT listener saw them (refused: no allowlist here).
    let refused: Vec<&str> = gw_logs
        .lines()
        .filter(|l| l.contains("antigravity-unleash.goog") || l.contains("play.googleapis.com"))
        .collect();
    run(&["network", "disconnect", "-f", &seat_net, &gw.name]);
    run(&["network", "rm", &seat_net]);
    run(&["rm", "-f", &stub, &gw.name]);
    run(&["network", "rm", &gw.external_network]);
    let (code, out, err, usage, ..) = turn;
    eprintln!(
        "before={before:?}\ntranscript={transcript}\nexit={exit:?}\nreport={report:?}\n\
         base_url={base_url:?}\nturn={code} in {took:?}\nout={out}\nerr={err}\nupstream={upstream}\n\
         foreign={foreign} unknown={unknown}\nafter={after:?}\nrefused={refused:#?}"
    );
    assert_eq!(before.0, FloorUnarmed::AgentSignedOut, "{before:?}");
    assert!(before.1.contains("agy is not signed in"), "{before:?}");
    assert!(transcript.contains("(it is not shown)"), "{transcript}");
    assert!(
        !transcript.contains("FAKE-agy-seat"),
        "the pasted key is not echoed: {transcript}"
    );
    assert_eq!(exit, Some(0), "{transcript}");
    assert_eq!(report.files, vec![GEMINI_KEY_FILE.to_string()]);
    assert!(
        login_gone,
        "the throwaway login volume is removed after ingest"
    );
    assert_eq!(base_url, Some(format!("http://agw:{GEMINI_PORT}")));
    assert!(
        unit_key.starts_with("wk-"),
        "the seat holds its unit key only"
    );
    assert_eq!(
        settings, r#"{"modelProvider":"gemini"}"#,
        "the floor's, unchanged"
    );
    assert_eq!(code, 0, "{out}\n{err}");
    assert_eq!(out.trim(), "VERDICT: PASS keyroute");
    assert_eq!(seen.completed, Some(true));
    assert!(usage.is_some());
    assert!(
        took < Duration::from_secs(45),
        "agy's refused hosts do not hold the turn: {took:?}"
    );
    let requests: Vec<serde_json::Value> = upstream
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .filter(|v: &serde_json::Value| v.get("m").is_some())
        .collect();
    assert!(
        !requests.is_empty(),
        "agy reached the Gemini API: {upstream}"
    );
    for r in &requests {
        assert_eq!(r["key"], FAKE_KEY, "the broker's key upstream: {r}");
    }
    assert!(
        !upstream.contains(&unit_key),
        "the unit key stays at the gateway"
    );
    assert_eq!(foreign, "401", "another agent's unit is refused");
    assert_eq!(unknown, "401");
    assert_eq!(after.0, FloorUnarmed::AgentSignedOut, "{after:?}");
    assert!(
        !gw_logs.contains("FAKE-agy-seat"),
        "no key in the gateway log"
    );
    // agy's own hosts (feature flags, telemetry) were tried through the CONNECT listener and
    // refused there, every one; the turn above completed anyway.
    assert!(!refused.is_empty(), "agy tried its other hosts: {gw_logs}");
    assert!(
        refused.iter().all(|l| l.contains("route not found")),
        "{refused:#?}"
    );
}
