//! Pure, per-unit CLI credential files. Only placeholders enter a seat; the gateway owns
//! upstream credentials (a claude OAuth seat is the one exception: it passes the operator's own
//! `claude setup-token` token through, from its environment, core#1024). Paths are relative to
//! the runner's isolated CLI home.

use std::path::{Path, PathBuf};

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

/// One brokered subscription provider of an agent (DES-SEAT-LOGIN-SUBSCRIPTIONS decision 1):
/// its own broker file, its own static gateway route and its own placeholder entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Subscription {
    /// pi's Anthropic (Claude Pro/Max) OAuth.
    PiAnthropic,
    /// pi's `openai-codex` (ChatGPT Plus/Pro) OAuth.
    PiChatgpt,
    /// pi's GitHub Copilot sign-in (the broker holds the exchanged Copilot token).
    PiCopilot,
    /// opencode's `openai` ChatGPT sign-in (`chatgpt-browser` / `chatgpt-headless`).
    OpencodeChatgpt,
    /// opencode's `github-copilot` device sign-in (the broker holds the GitHub token).
    OpencodeCopilot,
    /// opencode's `anthropic` API key (opencode 2.0.25 has no Claude subscription sign-in).
    OpencodeAnthropicKey,
    /// The copilot CLI's GitHub sign-in (`copilot login`); its seat runs the CLI's BYOK mode
    /// against the Copilot route, so no GitHub credential enters the seat.
    Copilot,
}

/// A subscription's static gateway route and broker files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SubscriptionRoute {
    pub(crate) sub: Subscription,
    pub(crate) cli: &'static str,
    pub(crate) provider: crate::seat_gateway::Provider,
    pub(crate) port: u16,
    /// The route's credential handling ([`crate::seat_gateway::render`]).
    pub(crate) kind: CredentialKind,
    /// The broker file the route serves (nonempty = signed in).
    pub(crate) file: &'static str,
    /// The header the unit's key rides; `None` = the kind's native credential header.
    pub(crate) unit_header: Option<&'static str>,
    /// The broker file holding the route's ChatGPT account id (a literal on the route).
    pub(crate) account_file: Option<&'static str>,
    /// The broker file holding the route's Copilot API host (a literal on the route).
    pub(crate) host_file: Option<&'static str>,
    /// The entry of the CLI's own credential record this route serves (pi's provider id,
    /// opencode's integration id).
    pub(crate) record_key: &'static str,
}

/// Every subscription route, in port order. Adding a provider is one row here.
pub(crate) const SUBSCRIPTION_ROUTES: &[SubscriptionRoute] = {
    use crate::seat_gateway::{Provider, UNIT_HEADER};
    &[
        SubscriptionRoute {
            sub: Subscription::PiAnthropic,
            cli: "pi",
            provider: Provider::Anthropic,
            port: 4006,
            kind: CredentialKind::ClaudeOauth,
            file: "pi.anthropic.token",
            unit_header: Some(UNIT_HEADER),
            account_file: None,
            host_file: None,
            record_key: "anthropic",
        },
        SubscriptionRoute {
            sub: Subscription::PiChatgpt,
            cli: "pi",
            provider: Provider::ChatGpt,
            port: 4007,
            kind: CredentialKind::CodexChatgpt,
            file: "pi.chatgpt.token",
            unit_header: Some(UNIT_HEADER),
            account_file: Some("pi.chatgpt.account"),
            host_file: None,
            record_key: "openai-codex",
        },
        SubscriptionRoute {
            sub: Subscription::PiCopilot,
            cli: "pi",
            provider: Provider::Copilot,
            port: 4008,
            kind: CredentialKind::ProviderKey,
            file: "pi.copilot.token",
            unit_header: Some(UNIT_HEADER),
            account_file: None,
            host_file: Some("pi.copilot.host"),
            record_key: "github-copilot",
        },
        SubscriptionRoute {
            sub: Subscription::OpencodeChatgpt,
            cli: "opencode",
            provider: Provider::ChatGpt,
            port: 4009,
            kind: CredentialKind::CodexChatgpt,
            file: "opencode.chatgpt.token",
            unit_header: Some(UNIT_HEADER),
            account_file: Some("opencode.chatgpt.account"),
            host_file: None,
            record_key: "openai",
        },
        SubscriptionRoute {
            sub: Subscription::OpencodeCopilot,
            cli: "opencode",
            provider: Provider::Copilot,
            port: 4010,
            kind: CredentialKind::ProviderKey,
            file: "opencode.copilot.token",
            unit_header: Some(UNIT_HEADER),
            account_file: None,
            host_file: Some("opencode.copilot.host"),
            record_key: "github-copilot",
        },
        SubscriptionRoute {
            sub: Subscription::OpencodeAnthropicKey,
            cli: "opencode",
            provider: Provider::Anthropic,
            port: 4011,
            kind: CredentialKind::ClaudeApiKey,
            file: "opencode.anthropic.key",
            unit_header: Some(UNIT_HEADER),
            account_file: None,
            host_file: None,
            record_key: "anthropic",
        },
        // The copilot CLI authenticates its unit on Authorization (its BYOK bearer env).
        SubscriptionRoute {
            sub: Subscription::Copilot,
            cli: "copilot",
            provider: Provider::Copilot,
            port: 4012,
            kind: CredentialKind::ProviderKey,
            file: "copilot.token",
            unit_header: None,
            account_file: None,
            // The account's Copilot API host, read by the broker with the token (OD-1).
            host_file: Some(crate::copilot_account::HOST_FILE),
            record_key: "github",
        },
    ]
};

impl Subscription {
    pub(crate) fn route(self) -> &'static SubscriptionRoute {
        SUBSCRIPTION_ROUTES
            .iter()
            .find(|r| r.sub == self)
            .expect("every subscription has a route")
    }

    /// The gateway origin the seat reaches this subscription's route at.
    pub(crate) fn gateway_url(self) -> String {
        format!("http://agw:{}", self.route().port)
    }
}

/// What a signed-in agent's seat is armed with: the native credential kind (claude, codex, and
/// pi's or opencode's provider API key), the brokered subscriptions it holds, or both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SeatSignIn {
    pub(crate) kind: Option<CredentialKind>,
    pub(crate) subscriptions: Vec<Subscription>,
}

impl From<CredentialKind> for SeatSignIn {
    fn from(kind: CredentialKind) -> Self {
        Self {
            kind: Some(kind),
            subscriptions: Vec::new(),
        }
    }
}

/// The constant ChatGPT access placeholder: an unsigned JWT carrying the placeholder account
/// (pi decodes the account from the access token; the unit itself rides `x-wicked-unit`).
fn chatgpt_placeholder_jwt() -> String {
    let encode = |v: &Value| URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap());
    let claims = json!({
        "exp": 4102444800_u64,
        "https://api.openai.com/auth": {"chatgpt_account_id": ACCOUNT_ID}
    });
    format!(
        "{}.{}.PLACEHOLDER-not-a-signature",
        encode(&json!({"alg": "none", "typ": "JWT"})),
        encode(&claims)
    )
}

/// Every placeholder file of a seat signed in with `sign_in`: the native kind's files
/// ([`placeholder_files`]) with each subscription's entries merged in. Only constant placeholders
/// land in a CLI's credential store; the unit's key is written beside each route's URL (pi's
/// `models.json` headers), never into the store (DES-SEAT-LOGIN-SUBSCRIPTIONS decision 3).
pub(crate) fn seat_files(
    cli: &str,
    sign_in: &SeatSignIn,
    unit_key: &str,
    gateway_url: &str,
) -> Vec<(PathBuf, String)> {
    let mut files = sign_in
        .kind
        .map(|kind| placeholder_files(cli, kind, unit_key, gateway_url))
        .unwrap_or_default();
    if cli == "opencode" {
        let files = if sign_in.subscriptions.is_empty() {
            files
        } else {
            opencode_files(files, &sign_in.subscriptions, unit_key)
        };
        return opencode_pin_to_routes(files);
    }
    if cli != "pi" {
        return files;
    }
    let mut take = |name: &str| -> Value {
        match files.iter().position(|(p, _)| p == Path::new(name)) {
            Some(i) => serde_json::from_str(&files.remove(i).1).unwrap_or_else(|_| json!({})),
            None => json!({}),
        }
    };
    let mut models = take("models.json");
    let mut auth = take("auth.json");
    for sub in &sign_in.subscriptions {
        let (id, entry, base) = match sub {
            Subscription::PiAnthropic => (
                "anthropic",
                json!({"type": "oauth", "access": "sk-ant-oat01-PLACEHOLDER-not-a-token",
                    "refresh": "PLACEHOLDER-refresh-not-a-token", "expires": 4102444800000_u64}),
                sub.gateway_url(),
            ),
            Subscription::PiChatgpt => (
                "openai-codex",
                json!({"type": "oauth", "access": chatgpt_placeholder_jwt(),
                    "refresh": "PLACEHOLDER-refresh-not-a-token", "expires": 4102444800000_u64,
                    "accountId": ACCOUNT_ID}),
                format!("{}/backend-api", sub.gateway_url()),
            ),
            // An api_key entry: pi's Copilot OAuth mode derives an https base URL from the token.
            Subscription::PiCopilot => (
                "github-copilot",
                json!({"type": "api_key", "key": "PLACEHOLDER-not-a-token"}),
                sub.gateway_url(),
            ),
            _ => continue,
        };
        auth[id] = entry;
        models["providers"][id] = json!({
            "baseUrl": base,
            "headers": {crate::seat_gateway::UNIT_HEADER: unit_key}
        });
    }
    if !sign_in.subscriptions.is_empty() || models != json!({}) {
        files.push((PathBuf::from("models.json"), models.to_string()));
    }
    // Always written: pi's run home persists across units, and a placeholder of a provider the
    // broker no longer holds must not survive into this one.
    files.push((PathBuf::from("auth.json"), auth.to_string()));
    files
}

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
        // Neither claude mode keeps an OAuth record in the seat. OAuth (core#1024): the seat's
        // claude passes the operator's `claude setup-token` token through, from its env
        // (`CLAUDE_CODE_OAUTH_TOKEN`), and the unit's key rides `x-wicked-unit`
        // ([`crate::seat_gateway::seat_env_for`]). API key: selected by ANTHROPIC_API_KEY. The
        // file is still written, empty, so a seat home an earlier unit used keeps no stale
        // record there (agy c6-r1).
        ("claude", CredentialKind::ClaudeOauth | CredentialKind::ClaudeApiKey) => {
            (".credentials.json", json!({}))
        }
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
                "apiKey": unit_key,
                "headers": {crate::seat_gateway::UNIT_HEADER: unit_key}
            }}}}),
        ),
        _ => return Vec::new(),
    };
    vec![(PathBuf::from(path), serde_json::to_string(&value).unwrap())]
}

/// (core#1016) The model each brokered opencode provider runs when the unit names none, in
/// preference order (the subscription routes' port order, then the key route). Each is in the
/// catalogue snapshot bundled with the pinned opencode (2.0.25), which the seat reads instead of
/// fetching `models.opencode.ai` ([`crate::seat_gateway::seat_env_for_sign_in`]).
pub(crate) const OPENCODE_ROUTE_MODELS: &[(&str, &str)] = &[
    ("openai", "openai/gpt-5.5"),
    ("github-copilot", "github-copilot/gpt-5-mini"),
    ("anthropic", "anthropic/claude-sonnet-5"),
];

/// (core#1016) Pin an opencode seat to its brokered routes: `enabled_providers` names only the
/// providers its `opencode.json` points at the gateway, and `model` / `small_model` default to one
/// of them ([`OPENCODE_ROUTE_MODELS`]). Without this, a unit that names no model ran opencode's
/// own default (its OpenCode Zen provider at `opencode.ai`, or a provider with no gateway base
/// URL such as `api.anthropic.com`): the gateway refused that direct egress, no request reached a
/// route, and every opencode unit exited 1 on ECONNRESET. A unit's own `-m` still wins, but only
/// among the enabled providers. A config with no gateway provider is left as it is.
fn opencode_pin_to_routes(mut files: Vec<(PathBuf, String)>) -> Vec<(PathBuf, String)> {
    let Some(i) = files
        .iter()
        .position(|(p, _)| p == Path::new("opencode.json"))
    else {
        return files;
    };
    let Ok(mut config) = serde_json::from_str::<Value>(&files[i].1) else {
        return files;
    };
    let routed: Vec<&str> = OPENCODE_ROUTE_MODELS
        .iter()
        .map(|(id, _)| *id)
        .filter(|id| config["provider"].get(*id).is_some())
        .collect();
    let Some(model) = routed.first().and_then(|first| {
        OPENCODE_ROUTE_MODELS
            .iter()
            .find(|(id, _)| id == first)
            .map(|(_, m)| *m)
    }) else {
        return files;
    };
    config["enabled_providers"] = json!(routed);
    config["model"] = json!(model);
    config["small_model"] = json!(model);
    // The seat's only egress is the gateway: no self-update and no session sharing upstream.
    config["autoupdate"] = json!(false);
    config["share"] = json!("disabled");
    files[i].1 = config.to_string();
    files
}

/// The opencode credential rows a seat's own server imports (`opencode auth import`), next to
/// its per-unit `opencode.json`: constant placeholders, identical for every unit, so concurrent
/// units sharing the seat's credential DB never overwrite each other (decision 3).
pub(crate) const OPENCODE_IMPORT_FILE: &str = "wicked-credentials.json";
/// The label of every row the engine places in a seat's opencode DB (ids `wicked-seat-…`): the
/// pinned-server script removes each engine row the unit's file does not name
/// (`worker_sandbox::opencode_pinned_server_argv`).
pub(crate) const OPENCODE_ROW_LABEL: &str = "wicked seat";

fn opencode_files(
    mut files: Vec<(PathBuf, String)>,
    subs: &[Subscription],
    unit_key: &str,
) -> Vec<(PathBuf, String)> {
    let mut config = match files
        .iter()
        .position(|(p, _)| p == Path::new("opencode.json"))
    {
        Some(i) => serde_json::from_str(&files.remove(i).1).unwrap_or_else(|_| json!({})),
        None => json!({}),
    };
    let header = json!({crate::seat_gateway::UNIT_HEADER: unit_key});
    let mut rows = Vec::new();
    for sub in subs {
        let (id, options, row) = match sub {
            Subscription::OpencodeChatgpt => (
                "openai",
                json!({"baseURL": format!("{}/backend-api/codex", sub.gateway_url()),
                    "headers": header}),
                Some(json!({"type": "oauth", "methodID": "chatgpt-headless",
                    "refresh": "PLACEHOLDER-refresh-not-a-token",
                    "access": chatgpt_placeholder_jwt(), "expires": 4102444800000_u64,
                    "metadata": {"accountID": ACCOUNT_ID}})),
            ),
            // opencode reads the Copilot API from the row's endpoint, the token from the row.
            Subscription::OpencodeCopilot => (
                "github-copilot",
                json!({"headers": header}),
                Some(json!({"type": "oauth", "methodID": "device",
                    "refresh": "PLACEHOLDER-not-a-token", "access": "PLACEHOLDER-not-a-token",
                    "expires": 0, "metadata": {"apiEndpoint": sub.gateway_url()}})),
            ),
            Subscription::OpencodeAnthropicKey => (
                "anthropic",
                json!({"baseURL": format!("{}/v1", sub.gateway_url()),
                    "apiKey": "PLACEHOLDER-not-a-key", "headers": header}),
                None,
            ),
            _ => continue,
        };
        config["provider"][id]["options"] = options;
        if let Some(value) = row {
            // Versioned by content: a changed placeholder is a new row (imported and activated)
            // and the old one is removed, never silently kept because its id exists.
            use sha2::{Digest, Sha256};
            let digest = Sha256::digest(value.to_string().as_bytes());
            let tag: String = digest[..4].iter().map(|b| format!("{b:02x}")).collect();
            rows.push(
                json!({"id": format!("wicked-seat-{id}-{tag}"), "integrationID": id,
                "label": OPENCODE_ROW_LABEL, "active": true, "value": value}),
            );
        }
    }
    // `opencode.json` stays first: the runner points OPENCODE_CONFIG at the first file.
    files.insert(0, (PathBuf::from("opencode.json"), config.to_string()));
    if !rows.is_empty() {
        files.push((
            PathBuf::from(OPENCODE_IMPORT_FILE),
            Value::Array(rows).to_string(),
        ));
    }
    files
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

    /// core#1024: a claude OAuth seat keeps no OAuth record of ours (the operator's own
    /// `claude setup-token` token rides its env); the unit's key is its `x-wicked-unit` header.
    #[test]
    fn claude_oauth_seat_holds_no_record_and_sends_the_unit_key_as_a_header() {
        assert_eq!(
            file("claude", CredentialKind::ClaudeOauth, ".credentials.json"),
            json!({})
        );
        assert_eq!(sent_bearer("claude", CredentialKind::ClaudeOauth, KEY), KEY);
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

    /// Decision 3: pi's credential store holds only constant placeholders; the unit's key is
    /// written once per route, as that route's `x-wicked-unit` header in `models.json`.
    #[test]
    fn pi_subscription_placeholders_are_constant_and_the_unit_rides_a_header() {
        let sign_in = SeatSignIn {
            kind: Some(CredentialKind::ProviderKey),
            subscriptions: vec![
                Subscription::PiAnthropic,
                Subscription::PiChatgpt,
                Subscription::PiCopilot,
            ],
        };
        let files = seat_files("pi", &sign_in, KEY, URL);
        assert_eq!(files.len(), 2);
        let get = |name: &str| -> Value {
            serde_json::from_str(&files.iter().find(|(p, _)| p == Path::new(name)).unwrap().1)
                .unwrap()
        };
        let auth = get("auth.json");
        assert!(
            !auth.to_string().contains(KEY),
            "the store never holds the unit key"
        );
        assert!(auth["anthropic"]["access"]
            .as_str()
            .unwrap()
            .contains("sk-ant-oat"));
        assert_eq!(auth["anthropic"]["expires"], 4102444800000_u64);
        assert_eq!(auth["openai-codex"]["accountId"], ACCOUNT_ID);
        let jwt = auth["openai-codex"]["access"].as_str().unwrap();
        let claims: Value = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(jwt.split('.').nth(1).unwrap())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            claims["https://api.openai.com/auth"]["chatgpt_account_id"],
            ACCOUNT_ID
        );
        assert_eq!(auth["github-copilot"]["type"], "api_key");
        let models = get("models.json");
        // The provider-key route keeps its own entry beside the subscriptions.
        assert_eq!(models["providers"]["openai"]["apiKey"], KEY);
        for (id, base) in [
            ("anthropic", "http://agw:4006"),
            ("openai-codex", "http://agw:4007/backend-api"),
            ("github-copilot", "http://agw:4008"),
        ] {
            assert_eq!(models["providers"][id]["baseUrl"], base, "{id}");
            assert_eq!(
                models["providers"][id]["headers"][crate::seat_gateway::UNIT_HEADER],
                KEY,
                "{id}"
            );
        }
        // Without subscriptions the key mode keeps its models.json, and auth.json is rewritten
        // empty: an earlier unit's subscription placeholders never survive in the run home.
        let mut expected = placeholder_files("pi", CredentialKind::ProviderKey, KEY, URL);
        expected.push((PathBuf::from("auth.json"), "{}".into()));
        let only_key = seat_files("pi", &CredentialKind::ProviderKey.into(), KEY, URL);
        assert_eq!(only_key.len(), 2);
        for (path, body) in &expected {
            let got = &only_key.iter().find(|(p, _)| p == path).unwrap().1;
            assert_eq!(
                serde_json::from_str::<Value>(got).unwrap(),
                serde_json::from_str::<Value>(body).unwrap()
            );
        }
    }

    /// opencode: its per-unit `opencode.json` (first: OPENCODE_CONFIG points at it) routes each
    /// provider with the unit's header; the DB rows it imports are constant (no unit material),
    /// so concurrent units sharing the seat's credential DB agree (decision 3).
    #[test]
    fn opencode_placeholders_route_by_header_and_import_constant_rows() {
        let sign_in = SeatSignIn {
            kind: None,
            subscriptions: vec![
                Subscription::OpencodeChatgpt,
                Subscription::OpencodeCopilot,
                Subscription::OpencodeAnthropicKey,
            ],
        };
        let files = seat_files("opencode", &sign_in, KEY, URL);
        assert_eq!(files[0].0, PathBuf::from("opencode.json"));
        let config: Value = serde_json::from_str(&files[0].1).unwrap();
        let header = crate::seat_gateway::UNIT_HEADER;
        assert_eq!(
            config["provider"]["openai"]["options"]["baseURL"],
            "http://agw:4009/backend-api/codex"
        );
        assert_eq!(
            config["provider"]["anthropic"]["options"]["baseURL"],
            "http://agw:4011/v1"
        );
        for id in ["openai", "github-copilot", "anthropic"] {
            assert_eq!(
                config["provider"][id]["options"]["headers"][header], KEY,
                "{id}"
            );
        }
        let (path, rows) = &files[1];
        assert_eq!(path, &PathBuf::from(OPENCODE_IMPORT_FILE));
        assert!(!rows.contains(KEY), "the rows hold no unit material");
        let other = seat_files("opencode", &sign_in, "wk-test-PLACEHOLDER-unit-2", URL);
        assert_eq!(&other[1].1, rows, "every unit imports the same rows");
        let rows: Value = serde_json::from_str(rows).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 2);
        assert_eq!(
            rows[1]["value"]["metadata"]["apiEndpoint"],
            "http://agw:4010"
        );
        assert_eq!(rows[0]["value"]["metadata"]["accountID"], ACCOUNT_ID);
        // Every row carries the engine's label and a content-versioned id (a changed placeholder
        // is a new row, never silently kept because its id exists); the pinned script removes
        // engine rows the unit's file does not name.
        for row in rows.as_array().unwrap() {
            assert_eq!(row["label"], OPENCODE_ROW_LABEL);
            let id = row["id"].as_str().unwrap();
            let integration = row["integrationID"].as_str().unwrap();
            let tag = id
                .strip_prefix(&format!("wicked-seat-{integration}-"))
                .unwrap();
            assert!(
                tag.len() == 8 && tag.bytes().all(|b| b.is_ascii_hexdigit()),
                "{id}"
            );
        }
        let key_only: SeatSignIn = CredentialKind::ProviderKey.into();
        let key_files = seat_files("opencode", &key_only, KEY, URL);
        let placed = placeholder_files("opencode", CredentialKind::ProviderKey, KEY, URL);
        assert_eq!(key_files.len(), 1);
        assert_eq!(key_files[0].0, placed[0].0);
        let (mut got, want): (Value, Value) = (
            serde_json::from_str(&key_files[0].1).unwrap(),
            serde_json::from_str(&placed[0].1).unwrap(),
        );
        assert_eq!(got["provider"], want["provider"]);
        assert_eq!(got["model"], "openai/gpt-5.5");
        got.as_object_mut().unwrap().retain(|k, _| k == "provider");
        assert_eq!(got, want, "the key route only gains the route pin");
        // The key route reads the unit from its header too, so a DB row (which opencode prefers
        // over the config key) can never stand in for this unit.
        let key_config: Value =
            serde_json::from_str(&seat_files("opencode", &key_only, KEY, URL)[0].1).unwrap();
        assert_eq!(
            key_config["provider"]["openai"]["options"]["headers"][header],
            KEY
        );
        let gw = crate::seat_gateway::GatewayConfig::default();
        let route = gw.providers.iter().find(|r| r.port == 4004).unwrap();
        assert_eq!(route.unit_header.as_deref(), Some(header));
    }

    /// (core#1016) An opencode seat runs only its brokered providers: `enabled_providers` names
    /// the providers its config points at the gateway, and `model` / `small_model` default to one
    /// of them, so a unit that names no model never runs opencode's own default (OpenCode Zen at
    /// `opencode.ai`, or a provider with no gateway base URL). Red on 0.14.1: no model and every
    /// provider enabled; every opencode unit died on ECONNRESET behind the gateway.
    #[test]
    fn an_opencode_seat_is_pinned_to_its_brokered_routes() {
        let config = |subs: Vec<Subscription>, kind: Option<CredentialKind>| -> Value {
            let files = seat_files(
                "opencode",
                &SeatSignIn {
                    kind,
                    subscriptions: subs,
                },
                KEY,
                URL,
            );
            assert_eq!(files[0].0, PathBuf::from("opencode.json"));
            serde_json::from_str(&files[0].1).unwrap()
        };
        for (subs, kind, enabled, model) in [
            (
                vec![Subscription::OpencodeAnthropicKey],
                None,
                json!(["anthropic"]),
                "anthropic/claude-sonnet-5",
            ),
            (
                vec![Subscription::OpencodeCopilot],
                None,
                json!(["github-copilot"]),
                "github-copilot/gpt-5-mini",
            ),
            (
                vec![
                    Subscription::OpencodeAnthropicKey,
                    Subscription::OpencodeChatgpt,
                ],
                None,
                json!(["openai", "anthropic"]),
                "openai/gpt-5.5",
            ),
            (
                Vec::new(),
                Some(CredentialKind::ProviderKey),
                json!(["openai"]),
                "openai/gpt-5.5",
            ),
        ] {
            let c = config(subs.clone(), kind);
            assert_eq!(c["enabled_providers"], enabled, "{subs:?}");
            assert_eq!(c["model"], model, "{subs:?}");
            assert_eq!(c["small_model"], model, "{subs:?}");
            assert_eq!(c["autoupdate"], false);
            assert_eq!(c["share"], "disabled");
            // Every enabled provider is one the config routes to the gateway.
            for id in c["enabled_providers"].as_array().unwrap() {
                let options = &c["provider"][id.as_str().unwrap()]["options"];
                assert_eq!(
                    options["headers"][crate::seat_gateway::UNIT_HEADER],
                    KEY,
                    "{id}"
                );
            }
        }
        // No gateway provider (an agent with no sign-in): nothing to pin, nothing written.
        assert!(seat_files(
            "opencode",
            &SeatSignIn {
                kind: None,
                subscriptions: Vec::new()
            },
            KEY,
            URL
        )
        .is_empty());
    }

    #[test]
    fn subscription_routes_are_unique_owned_served_and_reachable() {
        let defaults = crate::seat_gateway::GatewayConfig::default();
        let mut ports = std::collections::BTreeSet::new();
        for r in SUBSCRIPTION_ROUTES {
            assert!(ports.insert(r.port), "{:?}", r.sub);
            assert_eq!(crate::seat_gateway::route_owner(r.file), Some(r.cli));
            for f in [Some(r.file), r.account_file, r.host_file]
                .into_iter()
                .flatten()
            {
                assert_eq!(crate::seat_gateway::route_owner(f), Some(r.cli), "{f}");
            }
            assert!(defaults.providers.iter().any(
                |p| p.port == r.port && p.credential_file == Path::new("/broker").join(r.file)
            ));
            assert!(crate::container_floor::GATEWAY_PORTS.contains(&r.port));
            assert_eq!(r.sub.route().port, r.port);
        }
        assert!(defaults
            .providers
            .iter()
            .all(|p| crate::container_floor::GATEWAY_PORTS.contains(&p.port)));
    }

    #[test]
    fn json_serialization_preserves_literal_placeholder_characters() {
        let unusual = "test-PLACEHOLDER-\"\\\n";
        let files = placeholder_files("codex", CredentialKind::CodexApiKey, unusual, URL);
        let value: Value = serde_json::from_str(&files[0].1).unwrap();
        assert_eq!(value["OPENAI_API_KEY"], unusual);
    }
}
