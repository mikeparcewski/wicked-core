//! The agy seat's carrier on the container floor: its tarball seat image, its pseudo-terminal
//! devices, its seat home, its stdin, and (LIVE) one agy seat answering in stream-json through the
//! floor against a fake Gemini API — no account and no real key anywhere.

use super::*;
use wicked_apps_core::spawn::SeatCli;

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "wicked-cf-agy-{tag}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

/// A wrapper as [`arm`] renders it, for the given mounts.
fn wrapper_for(mounts: &[Mount]) -> Vec<String> {
    run_argv(&ContainerLaunch {
        runtime: Path::new("/usr/bin/docker"),
        image: "sha256:img",
        name: "wicked-seat-agy-run-1-abcd1234",
        network: "none",
        workdir: Path::new("/w"),
        user: "501:20",
        mounts,
        floor: "sha256:floor",
        memory: "3g",
        gateway_ports: GATEWAY_PORTS,
        own_ports: &[],
    })
}

/// A seat command: `wrapper` then the CLI argv.
fn seat_cmd(wrapper: &[String], cli: &[&str]) -> Command {
    // spawn-audit: test-only — a seat argv for the floor's predicates; the live test spawns one
    // only through `run_bounded`, whose `finalize` rebuilds it hardened.
    let mut c = Command::new(&wrapper[0]);
    c.args(&wrapper[1..]);
    c.args(cli);
    c
}

fn args_of(c: &Command) -> Vec<String> {
    c.get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

static OTHER_BUILD: SeatTarball = SeatTarball {
    version: "1.4.2",
    build: "4835981233356800",
    member: "antigravity",
    amd64: (
        "https://storage.googleapis.com/antigravity-public/antigravity-cli/1.4.2-4835981233356800/linux-x64/cli_linux_x64.tar.gz",
        "0000000000000000000000000000000000000000000000000000000000000000",
    ),
    arm64: AGY_TARBALL.arm64,
};

/// The agy seat image installs Google's release tarball for the host's architecture at the pinned
/// version, build and sha256, and the Dockerfile checks the sha256 before it unpacks anything,
/// fetches over https only, and installs the binary root-owned and read-only. Another checksum is
/// another image; the npm seats keep their package.
#[test]
fn the_agy_seat_image_installs_its_pinned_tarball_verified_before_unpacking() {
    let Some(SeatPackage::Tarball(t)) = seat_package("agy") else {
        panic!("agy is installed from its tarball");
    };
    assert_eq!((t.version, t.member), ("1.4.2", "antigravity"));
    for ((url, sum), leaf) in [
        (t.amd64, "linux-x64/cli_linux_x64.tar.gz"),
        (t.arm64, "linux-arm/cli_linux_arm64.tar.gz"),
    ] {
        assert!(
            url.starts_with("https://storage.googleapis.com/antigravity-public/antigravity-cli/"),
            "{url}"
        );
        assert!(
            url.ends_with(&format!("/{}-{}/{leaf}", t.version, t.build)),
            "{url}"
        );
        assert!(
            sum.len() == 64 && sum.bytes().all(|b| b.is_ascii_hexdigit()),
            "{sum}"
        );
    }
    assert!(!t.member.contains('/') && !t.member.contains(".."));
    let pkg = SeatPackage::Tarball(t);
    assert_eq!(pkg.version(), Some("1.4.2"));
    let args = pkg.build_args();
    for want in [
        format!("CLI_TARBALL_MEMBER={}", t.member),
        format!("CLI_TARBALL_URL_AMD64={}", t.amd64.0),
        format!("CLI_TARBALL_SHA256_AMD64={}", t.amd64.1),
        format!("CLI_TARBALL_URL_ARM64={}", t.arm64.0),
        format!("CLI_TARBALL_SHA256_ARM64={}", t.arm64.1),
        format!("CLI_PIN={}", pkg.pin()),
    ] {
        assert!(args.contains(&want), "{want} missing from {args:?}");
    }
    assert!(
        !args.iter().any(|a| a.starts_with("CLI_PACKAGE=")),
        "{args:?}"
    );
    assert!(seat_package("claude")
        .unwrap()
        .build_args()
        .iter()
        .any(|a| a.starts_with("CLI_PACKAGE=@anthropic-ai/claude-code@")));
    // The Dockerfile: the checksum before the unpack, https only, a read-only root-owned binary.
    let df = SEAT_DOCKERFILE;
    let verify = df.find("sha256sum -c -").expect("the checksum is verified");
    let unpack = df.find("tar -xzf").expect("the archive is unpacked");
    assert!(verify < unpack, "verified before anything is unpacked");
    assert!(df.contains("--proto '=https'"));
    assert!(df.contains(
        r#"install -o root -g root -m 0555 "$d/$CLI_TARBALL_MEMBER" "/usr/local/bin/$CLI""#
    ));
    assert!(df.contains("amd64) url=\"$CLI_TARBALL_URL_AMD64\"; sum=\"$CLI_TARBALL_SHA256_AMD64\""));
    assert!(df.contains("arm64) url=\"$CLI_TARBALL_URL_ARM64\"; sum=\"$CLI_TARBALL_SHA256_ARM64\""));
    // Every byte that identifies the release is in the image tag.
    let other = SeatPackage::Tarball(&OTHER_BUILD);
    assert!(pkg.pin().contains(t.amd64.1) && pkg.pin().contains(t.arm64.1));
    assert_ne!(
        content_tag(&[SEAT_DOCKERFILE, &pkg.pin(), DEFAULT_BASE]),
        content_tag(&[SEAT_DOCKERFILE, &other.pin(), DEFAULT_BASE]),
        "another checksum is another image"
    );
}

/// Only an agy seat's Landlock policy admits the pseudo-terminal devices its shell tool needs
/// (`/dev/ptmx`, `/dev/pts` read-write); `/dev` stays read-only, the wrapper stays one the reap and
/// [`finalize`] recognise, and a wrapper that is not the engine's is refused.
#[test]
fn only_an_agy_seat_gets_the_pseudo_terminal_devices() {
    for cli in ["claude", "codex", "pi", "opencode", "copilot"] {
        let mut w = wrapper_for(&[]);
        let before = w.clone();
        grant_seat_devices(&mut w, cli).unwrap();
        assert_eq!(w, before, "{cli} gets no device");
    }
    let mut w = wrapper_for(&[]);
    grant_seat_devices(&mut w, "agy").unwrap();
    let args = &w[1..];
    let img = image_index(args).expect("our wrapper");
    let end = landrun_end(args, img).expect("still our landrun options");
    let policy = &args[img..end];
    for dev in ["/dev/ptmx", "/dev/pts"] {
        assert!(
            policy.windows(2).any(|p| p == ["--rw", dev]),
            "{dev}: {policy:?}"
        );
    }
    assert!(policy.windows(2).any(|p| p == ["--ro", "/dev"]));
    assert!(policy.windows(2).any(|p| p == ["--rw", "/dev/null"]));
    assert_eq!(
        &args[end + 1..end + 5],
        [SEAT_ENTRY, "--check", CANARY_FILE, "--"]
    );
    assert!(seat_container(&seat_cmd(&w, &["agy", "-p=x"])).is_some());
    let mut foreign = s(&["/usr/bin/docker", "run", "img", "agy"]);
    assert!(grant_seat_devices(&mut foreign, "agy").is_err());
}

/// agy's configuration home IS its home: an agy seat runs with the carrier's seat home (writable
/// in the container), so its conversations survive into the next unit's container. Every other
/// seat keeps the tmpfs home, and so does an agy seat whose home the container does not mount
/// writable. The value is a path, never a secret, and `HOME` is never passed by name.
#[test]
fn an_agy_seat_runs_with_its_own_seat_home_and_no_other_seat_does() {
    let base = tmp("home");
    let home = base.join("seat-home");
    let tree = base.join("wt");
    let elsewhere = base.join("elsewhere");
    for d in [&home, &tree, &elsewhere] {
        std::fs::create_dir_all(d).unwrap();
    }
    let rw = vec![
        Mount::Bind {
            path: tree.clone(),
            rw: true,
        },
        Mount::Bind {
            path: home.clone(),
            rw: true,
        },
    ];
    let ro = vec![Mount::Bind {
        path: home.clone(),
        rw: false,
    }];
    let finalized = |cli: &str, home_env: &Path, mounts: &[Mount]| -> Vec<String> {
        let mut c = seat_cmd(&wrapper_for(mounts), &[cli, "-p=x"]);
        c.env("HOME", home_env);
        let fin = finalize(c);
        // The runtime client keeps the daemon's own HOME (its configuration and context).
        assert!(
            !fin.get_envs().any(|(k, _)| k == "HOME"),
            "the client's HOME is never the seat's"
        );
        args_of(&fin)
    };
    let ours = format!("HOME={}", home.display());
    let agy = finalized("agy", &home, &rw);
    assert!(agy.contains(&ours), "{agy:?}");
    assert!(!agy.iter().any(|a| a == SEAT_HOME_ENV), "{agy:?}");
    let img = image_index(&agy).unwrap();
    assert!(
        !agy[..img].windows(2).any(|w| w == ["--env", "HOME"]),
        "never by name"
    );
    for cli in ["claude", "codex", "pi", "copilot", "opencode"] {
        let other = finalized(cli, &home, &rw);
        assert!(other.iter().any(|a| a == SEAT_HOME_ENV), "{cli}: {other:?}");
        assert!(!other.contains(&ours), "{cli}");
    }
    let unmounted = finalized("agy", &elsewhere, &rw);
    assert!(unmounted.iter().any(|a| a == SEAT_HOME_ENV));
    let read_only = finalized("agy", &home, &ro);
    assert!(read_only.iter().any(|a| a == SEAT_HOME_ENV));
    // Writable above, but a deeper read-only bind or a mask over it: the tmpfs.
    let covered = |m: Mount| {
        let mut v = vec![Mount::Bind {
            path: base.clone(),
            rw: true,
        }];
        v.push(m);
        finalized("agy", &home, &v)
    };
    assert!(covered(Mount::Bind {
        path: home.clone(),
        rw: false
    })
    .iter()
    .any(|a| a == SEAT_HOME_ENV));
    assert!(covered(Mount::Tmpfs { path: home.clone() })
        .iter()
        .any(|a| a == SEAT_HOME_ENV));
    assert!(
        covered(Mount::Bind {
            path: tree.clone(),
            rw: false
        })
        .contains(&ours),
        "a read-only bind beside the home does not cover it"
    );
    // No carrier home at all: the tmpfs.
    let bare = args_of(&finalize(seat_cmd(&wrapper_for(&rw), &["agy", "-p=x"])));
    assert!(bare.iter().any(|a| a == SEAT_HOME_ENV));
    // Its placeholder files go to that home (the API-key and sign-in paths write there).
    assert_eq!(config_home_env("agy"), Some("HOME"));
    let _ = std::fs::remove_dir_all(&base);
}

/// An agy seat's stdin is a pipe (closed at once by the runner), never `/dev/null`: on the host,
/// and inside a seat container, whose runtime client passes its stdin through. A prompt that names
/// agy does not make another CLI agy.
#[test]
fn an_agy_seat_takes_its_stdin_from_a_pipe() {
    let w = wrapper_for(&[]);
    assert!(w.iter().any(|a| a == "--interactive"));
    assert!(stdin_must_be_a_pipe(&seat_cmd(&w, &["agy", "-p=x"])));
    assert!(!stdin_must_be_a_pipe(&seat_cmd(
        &w,
        &["claude", "-p", "agy"]
    )));
    assert!(!stdin_must_be_a_pipe(&seat_cmd(
        &w,
        &["codex", "exec", "--", "agy"]
    )));
    let bare = |program: &str| {
        // spawn-audit: test-only — commands built only for the predicate; none is spawned.
        Command::new(program)
    };
    assert!(stdin_must_be_a_pipe(&bare("/usr/local/bin/agy")));
    assert!(stdin_must_be_a_pipe(&bare("agy")));
    assert!(!stdin_must_be_a_pipe(&bare("claude")));
    let mut not_ours = bare("/usr/bin/docker");
    not_ours.args(["run", "img", "agy"]);
    assert!(!stdin_must_be_a_pipe(&not_ours));
}

/// An online agy seat has its gateway route (the Gemini API-key route); a seat whose agent has
/// none is still refused before anything is set up.
#[test]
fn an_online_agy_seat_has_its_gemini_route() {
    assert!(gateway_route_defined("agy", true).is_ok());
    assert!(gateway_route_defined("agy", false).is_ok());
    let err = gateway_route_defined("gemini", true).unwrap_err();
    assert!(err.contains("gemini"), "{err}");
    assert!(GATEWAY_PORTS.contains(&crate::seat_gateway::GEMINI_PORT));
    assert_eq!(SeatCli::from_binary("agy").root_name(), Some("agy"));
}

/// The floor writes agy's API-key settings into the seat's own agy home, never through a link a
/// seat planted there, and returns the canonical path it binds read-only; another sign-in kind,
/// or a seat with no agy home, is refused.
#[test]
fn the_floor_writes_agys_api_key_settings_and_nothing_through_a_link() {
    use crate::seat_credentials::CredentialKind;
    let base = tmp("settings");
    let home = base.join("seat");
    let agy = home.join(".gemini/antigravity-cli");
    std::fs::create_dir_all(&agy).unwrap();
    let dirs = vec![home.clone(), agy.clone(), home.join(".gemini")];
    let path = agy_floor_settings(&dirs, Some(CredentialKind::GeminiApiKey)).unwrap();
    assert_eq!(path, agy.join("settings.json").canonicalize().unwrap());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        r#"{"modelProvider":"gemini"}"#
    );
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    // A seat rewrote it: the floor writes its own again.
    std::fs::write(&path, r#"{"modelProvider":"other"}"#).unwrap();
    agy_floor_settings(&dirs, Some(CredentialKind::GeminiApiKey)).unwrap();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        r#"{"modelProvider":"gemini"}"#
    );
    // A seat planted a link there: refused, the link's target untouched.
    let target = base.join("host-file");
    std::fs::write(&target, "host").unwrap();
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    assert!(agy_floor_settings(&dirs, Some(CredentialKind::GeminiApiKey)).is_err());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "host");
    assert!(agy_floor_settings(&dirs, Some(CredentialKind::ClaudeApiKey)).is_err());
    assert!(agy_floor_settings(&dirs, None).is_err());
    assert!(agy_floor_settings(
        std::slice::from_ref(&home),
        Some(CredentialKind::GeminiApiKey)
    )
    .is_err());
    let _ = std::fs::remove_dir_all(&base);
}

/// Removes a test container by name however the test ends (a failed assertion included).
struct RemoveContainer(Runtime, String);

impl Drop for RemoveContainer {
    fn drop(&mut self) {
        let mut rm = self.0.cmd();
        rm.args(["rm", "--force", &self.1]);
        let _ = output_within(rm, REAP_TIMEOUT);
    }
}

/// The fake Gemini API the live test runs (`tests/fixtures/agy`), test-only.
const FAKE_API: &str = include_str!("../tests/fixtures/agy/fake-gemini-api.js");

/// LIVE (`WICKED_CONTAINER_LIVE=1` + a container runtime; skipped otherwise): the agy seat image
/// built from its pinned tarball, and agy seats run through the real floor (the trusted launcher,
/// Landlock, the operator's uid, a read-only root) against the agy spike's fake Gemini API in a
/// sidecar whose loopback the seat shares (on a gateway port, the only TCP the seat's policy
/// admits). No network, no account, a fake key. Proves, in order:
/// * the image holds agy 1.4.2, root-owned, mode 0555;
/// * without the device grant agy's shell tool cannot run (no PTY); with it, it runs;
/// * a turn's stream-json reads as the engine needs it: the final answer, per-step usage, the tool,
///   the conversation id, a completed turn;
/// * the seat home is the carrier's: the next unit's container resumes the conversation
///   (`--conversation`), and the resumed turn's usage is its own, not the conversation's total;
/// * a prompt that starts with `--` reaches agy as the prompt (`-p=<prompt>`);
/// * a turn its `--print-timeout` cuts off exits 0 and is settled as not done.
#[test]
fn live_an_agy_seat_answers_through_the_floor_in_stream_json() {
    if std::env::var("WICKED_CONTAINER_LIVE").ok().as_deref() != Some("1") {
        eprintln!("skipped: set WICKED_CONTAINER_LIVE=1 with a container runtime to run");
        return;
    }
    let rt = runtime().expect("a container runtime answers");
    let floor = ensure_floor_image(&rt).expect("the floor image");
    let image = ensure_seat_image(&rt, "agy").expect("the agy seat image");
    // The binary: the pinned version, root-owned and read-only.
    let mut probe = rt.cmd();
    probe.args([
        "run",
        "--rm",
        "--network",
        "none",
        "--entrypoint",
        "/bin/sh",
        &image,
        "-c",
        "stat -c '%U %a' /usr/local/bin/agy; HOME=/tmp agy --version",
    ]);
    let out = output_within(probe, Duration::from_secs(60)).expect("the image runs");
    let facts = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(facts.starts_with("root 555\n"), "{facts}");
    assert!(facts.contains("1.4.2"), "{facts}");

    let base = tmp("live");
    let tree = base.join("wt");
    let home = base.join("home");
    let api = base.join("api");
    for d in [&tree, &home, &api] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(api.join("fake-gemini-api.js"), FAKE_API).unwrap();
    let log = api.join("fake.log");
    // agy takes the Gemini API when its settings say so (the API-key path's floor writes this
    // file read-only; here the test stands in for it).
    let settings = home.join(".gemini/antigravity-cli");
    std::fs::create_dir_all(&settings).unwrap();
    std::fs::write(
        settings.join("settings.json"),
        r#"{"modelProvider":"gemini"}"#,
    )
    .unwrap();
    let user = operator_user();
    let api_name = format!("{}-api", container_name("agy", "live", 0));
    let mut sidecar = rt.cmd();
    sidecar.args([
        "run",
        "-d",
        "--rm",
        "--name",
        &api_name,
        "--label",
        &format!("{SEAT_LABEL}=1"),
        "--network",
        "none",
        "--read-only",
        "--user",
        &user,
        "--mount",
        &format!("type=bind,src={a},dst={a}", a = api.display()),
        "--env",
        "PORT=4000",
        "--env",
        &format!("SRV_LOG={}", log.display()),
        "--env",
        &format!("FAKE_CWD={}", tree.display()),
        "--entrypoint",
        "node",
        &image,
        &api.join("fake-gemini-api.js").to_string_lossy(),
    ]);
    let _sidecar = RemoveContainer(rt.clone(), api_name.clone());
    let started = output_within(sidecar, PROBE_TIMEOUT).expect("the fake API starts");
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let ready = Instant::now();
    while !std::fs::read_to_string(&log).is_ok_and(|l| l.contains("ready")) {
        assert!(
            ready.elapsed() < PROBE_TIMEOUT,
            "the fake API never listened"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let spec = SeatFloor {
        profile: "governed".into(),
        tree: tree.clone(),
        writable: vec![tree.clone(), home.clone()],
        protected: vec![],
        readmitted: vec![],
        readmitted_files: vec![],
        masks: vec![],
        pinned: vec![],
        net: vec![],
    };
    let mounts = mount_plan(&spec).unwrap();
    let network = format!("container:{api_name}");
    let mut n = 0;
    let mut turn = |prompt: &str, prior: Option<&str>, timeout: Duration, devices: bool| {
        n += 1;
        let name = container_name("agy", "live", n);
        let mut w = run_argv(&ContainerLaunch {
            runtime: &rt.cli,
            image: &image,
            name: &name,
            network: &network,
            workdir: &tree,
            user: &user,
            mounts: &mounts,
            floor: &floor,
            memory: "3g",
            gateway_ports: GATEWAY_PORTS,
            own_ports: &[],
        });
        if devices {
            grant_seat_devices(&mut w, "agy").unwrap();
        }
        let mut argv = crate::execute_wrapped::build_argv("agy -p \"{PROMPT}\"", prompt, &[]);
        assert!(crate::execute_wrapped::inject_agy_headless(
            &mut argv, prompt, timeout
        ));
        crate::headless::inject_session(SeatCli::Agy, &mut argv, prior, None);
        assert!(crate::worker_sandbox::agy_permissions_under_floor(
            &mut argv
        ));
        let mut cmd = seat_cmd(&w, &[]);
        cmd.args(&argv);
        cmd.current_dir(&tree);
        cmd.env("HOME", &home);
        cmd.env("GEMINI_API_KEY", "fake-agy-seat-key");
        cmd.env("GOOGLE_GEMINI_BASE_URL", "http://127.0.0.1:4000");
        let (tap, seen) = crate::headless::SessionTap::new(
            Box::<crate::execute_wrapped::AgyStreamJson>::default(),
            SeatCli::Agy,
        );
        let r = crate::execute_wrapped::run_bounded(
            cmd,
            timeout + Duration::from_secs(60),
            &|_| {},
            Box::new(tap),
            None,
        )
        .expect("the seat ran");
        let seen = seen.lock().unwrap().clone();
        eprintln!(
            "--- turn {n} ({prompt}): exit {} ---\n{}\n{}",
            r.0, r.1, r.2
        );
        (r, seen)
    };
    let minute = Duration::from_secs(60);
    let marker = tree.join("agy-tool.txt");
    // No device grant: agy's shell tool has no PTY, the command never runs.
    let ((code, _, err, ..), _) = turn("AGY-TOOL AGY-ECHO=nopty", None, minute, false);
    assert_eq!(code, 0, "{err}");
    assert!(!marker.exists(), "no PTY, no command");
    // With it: the tool runs, and the turn reads as the engine needs it.
    let ((code, out, err, usage, files, tools), seen) =
        turn("AGY-TOOL AGY-ECHO=live1", None, minute, true);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(
        err.contains("wicked-seat-launch: floor canary passed"),
        "{err}"
    );
    assert_eq!(out.trim(), "VERDICT: PASS live1", "the final answer alone");
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap().trim(),
        "agy-tool-ran",
        "the shell tool ran on its PTY, in the tree"
    );
    assert!(tools.iter().any(|t| t == "run_command"), "{tools:?}");
    assert!(files.is_empty(), "{files:?}");
    let usage = usage.expect("usage");
    assert!(
        usage.input_tokens > 0 && usage.output_tokens > 0,
        "{usage:?}"
    );
    assert_eq!(seen.completed, Some(true));
    let id = seen.id.clone().expect("the conversation id");
    assert!(crate::headless::valid_session_id(SeatCli::Agy, &id), "{id}");
    assert!(
        std::fs::read_dir(home.join(".gemini/antigravity-cli"))
            .unwrap()
            .count()
            > 1,
        "agy's state is on the seat home"
    );
    // A new container resumes it from the seat home; its usage is this turn's own.
    let ((code, out, err, usage, ..), seen2) = turn("AGY-ECHO=live2", Some(&id), minute, true);
    assert_eq!(code, 0, "{out}\n{err}");
    assert_eq!(out.trim(), "VERDICT: PASS live2");
    assert_eq!(
        seen2.id.as_deref(),
        Some(id.as_str()),
        "the same conversation"
    );
    let resumed = std::fs::read_to_string(&log).unwrap();
    assert!(
        resumed
            .lines()
            .any(|l| l.contains("AGY-ECHO=live1") && l.contains("AGY-ECHO=live2")),
        "the resumed request carries turn 1:\n{resumed}"
    );
    // The fake's one main-model call: 100 prompt tokens (40 cached) and 5 + 3 thinking out.
    let usage = usage.expect("usage");
    assert_eq!(
        (
            usage.input_tokens,
            usage.cache_read_tokens,
            usage.output_tokens
        ),
        (100, 40, 8),
        "one call's usage, not the conversation's total"
    );
    // A prompt that starts with `--` is the prompt.
    let ((code, out, err, ..), _) = turn("--model=x AGY-ECHO=live3", None, minute, true);
    assert_eq!(code, 0, "{out}\n{err}");
    assert_eq!(out.trim(), "VERDICT: PASS live3");
    // agy's own --print-timeout cuts a turn off: exit 0, which the carrier settles as not done.
    let ((mut code, mut out, err, ..), seen) =
        turn("AGY-SLOW", None, Duration::from_secs(10), true);
    assert_eq!(code, 0, "agy reports a cut-off turn as success: {err}");
    assert!(crate::execute_wrapped::agy_print_timed_out(&err), "{err}");
    crate::headless::settle_agy_print_timeout(&err, &mut code, &mut out);
    crate::headless::settle_cut_off_turn(&seen, &mut code, &mut out, "agy");
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("--print-timeout"), "{out}");
    drop(_sidecar);
    let _ = std::fs::remove_dir_all(&base);
}
