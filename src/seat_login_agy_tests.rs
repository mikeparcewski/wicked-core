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

/// The login command runs the paste script, saving the key 0600 under the throwaway home.
/// The script itself (run here without a terminal) saves
/// exactly the key and prints none of it.
#[test]
fn agys_login_reads_the_key_unechoed_into_the_throwaway_volume() {
    let command = login_command("agy");
    assert_eq!(&command[..2], &["sh", "-c"]);
    assert_eq!(command[2], AGY_API_KEY_LOGIN);
    assert!(command[2].contains("stty -echo"));
    assert!(command[2].contains("(it is not shown)"));
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
