//! Shared agy seat image, pseudo-terminal policy, stdin, and API-key settings tests.

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
/// (`/dev/ptmx`, `/dev/pts` read-write); the base policy keeps `/dev` read-only.
#[test]
fn only_an_agy_seat_gets_the_pseudo_terminal_devices() {
    for cli in ["claude", "codex", "pi", "opencode", "copilot"] {
        assert!(seat_devices(cli).is_empty(), "{cli} gets no devices");
    }
    assert_eq!(seat_devices("agy"), &["/dev/ptmx", "/dev/pts"]);
    let policy = landrun_prefix(&[], &[], &[]);
    assert!(policy.windows(2).any(|p| p == ["--ro", "/dev"]));
    assert!(policy.windows(2).any(|p| p == ["--rw", "/dev/null"]));
    // The assembled launch policy (B-6 review): agy's devices are granted before landrun's own
    // `--` separator, and no other CLI gets them.
    let sep = |v: &[String]| {
        v.iter()
            .enumerate()
            .filter(|(_, a)| *a == "--")
            .nth(1)
            .unwrap()
            .0
    };
    let agy = seat_launch_policy("agy", landrun_prefix(&[], &[], &[]));
    let at = sep(&agy);
    for d in ["/dev/ptmx", "/dev/pts"] {
        let i = agy
            .windows(2)
            .position(|p| p == ["--rw", d])
            .unwrap_or_else(|| panic!("{d} is not granted: {agy:?}"));
        assert!(i < at, "{d} is granted after the separator: {agy:?}");
    }
    let codex = seat_launch_policy("codex", landrun_prefix(&[], &[], &[]));
    assert!(
        !codex.iter().any(|a| a == "/dev/ptmx" || a == "/dev/pts"),
        "{codex:?}"
    );
    assert_eq!(codex.len() + 4, agy.len());
}

/// An agy CLI needs a pipe for stdin; mentioning agy in another CLI's prompt does not.
#[test]
fn an_agy_seat_takes_its_stdin_from_a_pipe() {
    // Predicate only: these commands are never spawned.
    assert!(stdin_must_be_a_pipe(&Command::new("/usr/local/bin/agy")));
    assert!(stdin_must_be_a_pipe(&Command::new("agy")));
    let mut claude = Command::new("claude");
    claude.args(["-p", "agy"]);
    assert!(!stdin_must_be_a_pipe(&claude));
    let mut codex = Command::new("codex");
    codex.args(["exec", "--", "agy"]);
    assert!(!stdin_must_be_a_pipe(&codex));
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
