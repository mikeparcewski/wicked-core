//! Pure, per-unit CLI credential files. Only placeholders enter a seat; the gateway owns
//! upstream credentials and refresh. Paths are relative to the runner's isolated CLI home.

use std::path::PathBuf;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum CredentialKind {
    ClaudeOauth,
    ClaudeApiKey,
    CodexChatgpt,
    CodexApiKey,
    ProviderKey,
}

const ACCOUNT_ID: &str = "acct-PLACEHOLDER";

/// The exact apiKey value to register, excluding the HTTP `Bearer ` prefix. Codex sends the
/// entire JWT, not just its per-unit signature. No JWT is signed by or trusted as a provider.
pub(crate) fn sent_bearer(cli: &str, kind: CredentialKind, unit_key: &str) -> String {
    if cli == "codex" && kind == CredentialKind::CodexChatgpt {
        let header = json!({"alg": "none", "typ": "JWT"});
        let claims = json!({
            "email": "seat@example.invalid",
            "exp": 4102444800_u64,
            "wicked_unit_key": unit_key,
            "https://api.openai.com/auth": {
                "chatgpt_plan_type": "pro", "chatgpt_account_id": ACCOUNT_ID
            }
        });
        let encode = |v: &Value| URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap());
        format!("{}.{}.{unit_key}", encode(&header), encode(&claims))
    } else {
        unit_key.into()
    }
}

/// Render the spike's credential shapes. `gateway_url` is the selected route's origin
/// (`http://agw:<port>`); provider clients append their own API paths.
pub(crate) fn placeholder_files(
    cli: &str,
    kind: CredentialKind,
    unit_key: &str,
    gateway_url: &str,
) -> Vec<(PathBuf, String)> {
    let (path, value) = match (cli, kind) {
        ("claude", CredentialKind::ClaudeOauth) => (
            ".credentials.json",
            json!({"claudeAiOauth": {
                "accessToken": unit_key,
                "refreshToken": "PLACEHOLDER-refresh-not-a-token",
                "expiresAt": 4102444800000_u64,
                "scopes": ["user:inference", "user:profile"],
                "subscriptionType": "max"
            }}),
        ),
        // Claude API-key mode is selected by ANTHROPIC_API_KEY, not an OAuth file. The file is
        // still written, with no OAuth record, so a seat home an earlier unit of the run used
        // under an OAuth sign-in keeps no stale placeholder there (agy c6-r1).
        ("claude", CredentialKind::ClaudeApiKey) => (".credentials.json", json!({})),
        ("codex", CredentialKind::CodexChatgpt) => {
            let jwt = sent_bearer(cli, kind, unit_key);
            (
                "auth.json",
                json!({
                    "auth_mode": "chatgpt", "OPENAI_API_KEY": null,
                    "tokens": {
                        "id_token": jwt, "access_token": jwt,
                        "refresh_token": "PLACEHOLDER-refresh", "account_id": ACCOUNT_ID
                    },
                    "last_refresh": "2099-01-01T00:00:00Z"
                }),
            )
        }
        ("codex", CredentialKind::CodexApiKey) => (
            "auth.json",
            json!({"auth_mode": "apikey", "OPENAI_API_KEY": unit_key,
                "tokens": null, "last_refresh": null}),
        ),
        ("pi", CredentialKind::ProviderKey) => (
            "models.json",
            json!({"providers": {"openai": {
                "baseUrl": gateway_url, "apiKey": unit_key, "api": "openai-responses",
                "authHeader": true
            }}}),
        ),
        ("opencode", CredentialKind::ProviderKey) => (
            "opencode.json",
            json!({"provider": {"openai": {"options": {
                "baseURL": format!("{}/v1", gateway_url.trim_end_matches('/')),
                "apiKey": unit_key
            }}}}),
        ),
        _ => return Vec::new(),
    };
    vec![(PathBuf::from(path), serde_json::to_string(&value).unwrap())]
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPIKE: &str = include_str!("../tests/fixtures/broker/spike-placeholders.sh");
    const KEY: &str = "wk-test-PLACEHOLDER-unit-1";
    const URL: &str = "http://agw:4003";

    fn file(cli: &str, kind: CredentialKind, path: &str) -> Value {
        let files = placeholder_files(cli, kind, KEY, URL);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, PathBuf::from(path));
        assert!(files[0].0.is_relative());
        serde_json::from_str(&files[0].1).unwrap()
    }

    #[test]
    fn claude_oauth_matches_spike_structure_and_sends_unit_key() {
        let line = SPIKE.lines().find(|l| l.contains("claudeAiOauth")).unwrap();
        let literal = line
            .split("printf '%s' '")
            .nth(1)
            .unwrap()
            .split("' >")
            .next()
            .unwrap();
        let mut expected: Value = serde_json::from_str(literal).unwrap();
        expected["claudeAiOauth"]["accessToken"] = json!(KEY);
        expected["claudeAiOauth"]["refreshToken"] = json!("PLACEHOLDER-refresh-not-a-token");
        let actual = file("claude", CredentialKind::ClaudeOauth, ".credentials.json");
        assert_eq!(actual, expected);
        assert_eq!(
            actual["claudeAiOauth"]["accessToken"],
            sent_bearer("claude", CredentialKind::ClaudeOauth, KEY)
        );
    }

    #[test]
    fn codex_chatgpt_matches_spike_structure_and_registers_the_whole_jwt() {
        let jwt = sent_bearer("codex", CredentialKind::CodexChatgpt, KEY);
        let line = SPIKE
            .lines()
            .find(|l| l.contains("printf '%s'") && l.contains("auth_mode"))
            .unwrap();
        let literal = line
            .split("printf '%s' \"")
            .nth(1)
            .unwrap()
            .split("\" >")
            .next()
            .unwrap();
        let expected: Value =
            serde_json::from_str(&literal.replace("\\\"", "\"").replace("$J", &jwt)).unwrap();
        let actual = file("codex", CredentialKind::CodexChatgpt, "auth.json");
        assert_eq!(actual, expected);
        assert_eq!(actual["tokens"]["access_token"], jwt);
        assert_eq!(actual["tokens"]["id_token"], jwt);
        let segments: Vec<_> = jwt.split('.').collect();
        assert_eq!(segments.len(), 3);
        assert_eq!(segments[2], KEY);
        let decode = |segment: &str| -> Value {
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(segment).unwrap()).unwrap()
        };
        assert_eq!(decode(segments[0]), json!({"alg":"none", "typ":"JWT"}));
        let claims = decode(segments[1]);
        assert_eq!(claims["exp"], 4102444800_u64);
        assert_eq!(claims["wicked_unit_key"], KEY);
        assert_eq!(
            claims["https://api.openai.com/auth"]["chatgpt_account_id"],
            actual["tokens"]["account_id"]
        );
        assert!(!segments[..2].iter().any(|s| s.contains('=')));
    }

    #[test]
    fn provider_files_hold_only_unit_keys_and_gateway_urls() {
        for (cli, path, pointer, base, expected_url) in [
            (
                "pi",
                "models.json",
                "/providers/openai/apiKey",
                "/providers/openai/baseUrl",
                URL,
            ),
            (
                "opencode",
                "opencode.json",
                "/provider/openai/options/apiKey",
                "/provider/openai/options/baseURL",
                "http://agw:4003/v1",
            ),
        ] {
            assert!(SPIKE.contains(path));
            assert!(SPIKE.contains(&format!("PLACEHOLDER-{cli}-key")));
            let actual = file(cli, CredentialKind::ProviderKey, path);
            assert_eq!(actual.pointer(pointer).unwrap(), KEY);
            assert_eq!(actual.pointer(base).unwrap(), expected_url);
            assert_eq!(sent_bearer(cli, CredentialKind::ProviderKey, KEY), KEY);
        }
    }

    #[test]
    fn api_key_modes_use_the_unit_key_without_oauth_tokens() {
        let claude = placeholder_files("claude", CredentialKind::ClaudeApiKey, KEY, URL);
        assert_eq!(claude.len(), 1);
        assert_eq!(claude[0].0, PathBuf::from(".credentials.json"));
        assert_eq!(claude[0].1, "{}", "no OAuth record, no key in the file");
        assert_eq!(
            sent_bearer("claude", CredentialKind::ClaudeApiKey, KEY),
            KEY
        );
        let codex = file("codex", CredentialKind::CodexApiKey, "auth.json");
        assert_eq!(
            codex["OPENAI_API_KEY"],
            sent_bearer("codex", CredentialKind::CodexApiKey, KEY)
        );
        assert_eq!(codex["auth_mode"], "apikey");
        assert!(codex["tokens"].is_null());
    }

    #[test]
    fn different_units_send_different_credentials_for_every_kind() {
        for (cli, kind) in [
            ("claude", CredentialKind::ClaudeOauth),
            ("claude", CredentialKind::ClaudeApiKey),
            ("codex", CredentialKind::CodexChatgpt),
            ("codex", CredentialKind::CodexApiKey),
            ("pi", CredentialKind::ProviderKey),
            ("opencode", CredentialKind::ProviderKey),
        ] {
            assert_ne!(
                sent_bearer(cli, kind, KEY),
                sent_bearer(cli, kind, "wk-test-PLACEHOLDER-unit-2")
            );
        }
    }

    #[test]
    fn json_serialization_preserves_literal_placeholder_characters() {
        let unusual = "test-PLACEHOLDER-\"\\\n";
        let files = placeholder_files("claude", CredentialKind::ClaudeOauth, unusual, URL);
        let value: Value = serde_json::from_str(&files[0].1).unwrap();
        assert_eq!(value["claudeAiOauth"]["accessToken"], unusual);
    }
}
