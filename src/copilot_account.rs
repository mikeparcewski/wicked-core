//! The Copilot account a copilot CLI sign-in may use: its Copilot API host and the models it may
//! run (DES-SEAT-LOGIN-SUBSCRIPTIONS OD-1, option a).
//!
//! The copilot CLI (1.0.96) learns both from the GitHub token itself, never from a constant:
//! - `GET https://api.github.com/copilot_internal/user` ("fetching Copilot user",
//!   `application/vnd.github+json`, `X-GitHub-Api-Version: 2022-11-28`); its `endpoints.api` is
//!   the account's Copilot API (`copilotUrl = settings.copilotUrl || COPILOT_API_URL ||
//!   copilotUser.endpoints.api`);
//! - `GET {endpoints.api}/models` ("fetched model catalog from CAPI"), sent with the CLI's
//!   integration id (`GITHUB_COPILOT_INTEGRATION_ID || "copilot-developer-cli"`). The CLI drops
//!   rows with `model_picker_enabled: false`.
//!
//! The broker does the same with the brokered token at sign-in and on its refresh pass, keeps
//! the result beside the token (`copilot.host`, `copilot.models`: identifiers, never
//! credentials), and publishes both as route literals: the gateway's copilot route targets that
//! host, and a copilot seat runs a model from that list. The seat still sees only placeholders.

// Native Windows has no container floor: only the pure half is used there.
#![cfg_attr(not(unix), allow(dead_code))]

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The broker file holding the account's Copilot API host (a route literal, like pi's).
pub(crate) const HOST_FILE: &str = "copilot.host";
/// The broker file holding the account's models ([`CopilotAccount`] as JSON).
pub(crate) const MODELS_FILE: &str = "copilot.models";
/// GitHub's endpoint naming the account's Copilot endpoints (the copilot CLI's own call).
pub(crate) const USER_URL: &str = "https://api.github.com/copilot_internal/user";
/// The copilot CLI's own Copilot integration id (its `GITHUB_COPILOT_INTEGRATION_ID` default).
/// The models are listed with it, and the copilot route sends it, so what the list allows is
/// what the route's requests may use.
pub(crate) const INTEGRATION_ID: &str = "copilot-developer-cli";
/// The headers of the account lookup (the CLI's "fetching Copilot user" request).
pub(crate) const USER_HEADERS: &[(&str, &str)] = &[
    ("accept", "application/vnd.github+json"),
    ("x-github-api-version", "2022-11-28"),
];
/// The headers of the model listing.
pub(crate) const MODELS_HEADERS: &[(&str, &str)] = &[
    ("accept", "application/json"),
    ("copilot-integration-id", INTEGRATION_ID),
];
/// How long a resolved account is reused before the refresh pass reads it again.
pub(crate) const RESOLVE_EVERY_SECS: u64 = 6 * 60 * 60;
/// The model a copilot seat runs when nothing selects one: the first of these the account may
/// use (models every paid plan includes), else the cheapest model by the account's own billing
/// data, else the account's own chat default, else its first small-tier model
/// ([`SMALL_MODEL_MARKERS`], in that order), else its first. An operator picks another with `--model <id>` in
/// the seat's `headless_invocation` (clis.toml).
pub(crate) const PREFERRED_MODELS: &[&str] = &["gpt-5-mini", "gpt-4.1", "gpt-4o"];
/// Name markers of a provider's small tier, in preference order: an account whose list names
/// none of [`PREFERRED_MODELS`] and states no billing (an enterprise plan's list, for one) runs
/// its lightest model rather than whichever model its list happens to name first.
pub(crate) const SMALL_MODEL_MARKERS: &[&str] = &["-mini", "haiku", "-flash", "-nano", "-lite"];
/// At most this many models are kept (a bound on the literal, not a plan limit).
const MAX_MODELS: usize = 256;

/// One model the account may use, with the API paths the Copilot API serves it on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CopilotModel {
    pub(crate) id: String,
    /// `supported_endpoints` (`/chat/completions`, `/responses`, …); empty = not stated.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) endpoints: Vec<String>,
    /// The premium-request multiplier (`billing.multiplier`), in thousandths; `None` = not stated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) multiplier: Option<u64>,
    /// A relative token price (`billing.token_prices`: input + output per batch); `None` = not
    /// stated. Only compared with other models of the same list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) price: Option<u64>,
}

/// What the broker read for a copilot sign-in: the models it may run, in the API's order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CopilotAccount {
    pub(crate) models: Vec<CopilotModel>,
    /// The account's own chat default (`is_chat_default`), when it is one of [`Self::models`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) default: Option<String>,
    /// When it was read (Unix seconds).
    pub(crate) resolved_at: u64,
}

fn plain_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._:/-".contains(c))
}

impl CopilotAccount {
    /// Reject anything that is not a short list of plain model ids before it becomes a literal.
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.models.len() > MAX_MODELS {
            return Err("copilot: the account lists too many models".into());
        }
        let ok = self.models.iter().all(|m| {
            plain_id(&m.id) && m.endpoints.len() <= 16 && m.endpoints.iter().all(|e| plain_id(e))
        });
        if !ok || self.default.as_deref().is_some_and(|d| !plain_id(d)) {
            return Err("copilot: the account's model list is not plain model ids".into());
        }
        Ok(())
    }

    fn get(&self, id: &str) -> Option<&CopilotModel> {
        self.models.iter().find(|m| m.id == id)
    }

    fn ids(&self) -> String {
        self.models
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The account's Copilot API host from the `copilot_internal/user` answer (pure): its
/// `endpoints.api`, which must be an https githubcopilot.com API host.
pub(crate) fn api_host(user: &Value) -> Result<String, String> {
    let api = crate::seat_login::nonempty(user.pointer("/endpoints/api")).ok_or(
        "copilot: GitHub names no Copilot API endpoint for this account (is Copilot enabled \
         for it?)",
    )?;
    let host = api
        .strip_prefix("https://")
        .ok_or("copilot: the account's Copilot API endpoint is not https")?
        .trim_end_matches('/');
    crate::seat_gateway::validate_copilot_host(host)?;
    Ok(host.to_string())
}

/// The models the account may use, from its `/models` answer (pure): chat models whose policy,
/// when stated, is `enabled`, and that the model picker does not hide (as the copilot CLI's
/// own catalog drops them). Anything else (embeddings, a disabled or unconfigured policy) is
/// refused by the Copilot API, so it is never offered to a seat.
pub(crate) fn account_models(list: &Value, now: u64) -> Result<CopilotAccount, String> {
    let rows = list
        .get("data")
        .and_then(Value::as_array)
        .ok_or("copilot: the Copilot API's model list has no data")?;
    let mut models: Vec<CopilotModel> = Vec::new();
    let mut default = None;
    for row in rows {
        let Some(id) = row
            .get("id")
            .and_then(Value::as_str)
            .filter(|i| plain_id(i))
        else {
            continue;
        };
        let chat = row
            .pointer("/capabilities/type")
            .and_then(Value::as_str)
            .is_none_or(|t| t == "chat");
        let enabled = row
            .pointer("/policy/state")
            .and_then(Value::as_str)
            .is_none_or(|s| s == "enabled");
        let shown = row.get("model_picker_enabled").and_then(Value::as_bool) != Some(false);
        if !(chat && enabled && shown) || models.iter().any(|m| m.id == id) {
            continue;
        }
        let endpoints = row
            .get("supported_endpoints")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .filter(|e| plain_id(e))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if row.get("is_chat_default").and_then(Value::as_bool) == Some(true) {
            default = Some(id.to_string());
        }
        let number = |p: &str| {
            row.pointer(p)
                .and_then(Value::as_f64)
                .filter(|n| n.is_finite() && *n >= 0.0)
        };
        let multiplier = number("/billing/multiplier").map(|m| (m * 1000.0).round() as u64);
        let price = number("/billing/token_prices/batch_size")
            .filter(|b| *b > 0.0)
            .and_then(|batch| {
                let input = number("/billing/token_prices/default/input_price")?;
                let output = number("/billing/token_prices/default/output_price")?;
                Some(((input + output) * 1_000_000.0 / batch).round() as u64)
            });
        models.push(CopilotModel {
            id: id.to_string(),
            endpoints,
            multiplier,
            price,
        });
        if models.len() == MAX_MODELS {
            break;
        }
    }
    let account = CopilotAccount {
        models,
        default,
        resolved_at: now,
    };
    account.validate()?;
    Ok(account)
}

/// The broker files a resolved account is kept in (pure).
pub(crate) fn account_files(
    host: &str,
    account: &CopilotAccount,
) -> Result<Vec<(String, Vec<u8>)>, String> {
    crate::seat_gateway::validate_copilot_host(host)?;
    account.validate()?;
    let json = serde_json::to_vec(account).map_err(|_| "copilot: could not encode the models")?;
    Ok(vec![
        (HOST_FILE.into(), host.as_bytes().to_vec()),
        (MODELS_FILE.into(), json),
    ])
}

/// The account a `copilot.models` broker file holds; `None` when absent or unreadable.
pub(crate) fn parse_models_file(bytes: &[u8]) -> Option<CopilotAccount> {
    let account: CopilotAccount = serde_json::from_slice(bytes).ok()?;
    account.validate().ok()?;
    Some(account)
}

/// The model a copilot BYOK seat sends upstream, resolved as copilot does (pure):
/// `COPILOT_PROVIDER_WIRE_MODEL`, else the last `--model X`/`--model=X` (never past `--`), else
/// `COPILOT_MODEL`, else `COPILOT_PROVIDER_MODEL_ID`. `None` = nothing selects one.
pub(crate) fn selected_model(
    args: &[String],
    env: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    let named = |k: &str| env(k).filter(|m| !m.is_empty());
    named("COPILOT_PROVIDER_WIRE_MODEL")
        .or_else(|| crate::seat_gateway::copilot_model_flag(args))
        .or_else(|| named("COPILOT_MODEL"))
        .or_else(|| named("COPILOT_PROVIDER_MODEL_ID"))
}

/// The model a copilot seat must be given, checked against the account (pure):
/// - `Ok(None)`: the seat already selects a model the account may use;
/// - `Ok(Some(id))`: nothing selects one, so the seat runs `id` (the preferred order);
/// - `Err(reason)`: refuse the unit, naming why (and the allowed models).
pub(crate) fn seat_model(
    selected: Option<&str>,
    account: Option<&CopilotAccount>,
) -> Result<Option<String>, String> {
    let account = account.ok_or(
        "copilot's sign-in has no resolved Copilot account yet: the broker reads its Copilot API \
         endpoint and allowed models at sign-in and on each refresh pass (the Agents tab shows \
         the last error)",
    )?;
    if account.models.is_empty() {
        return Err("this Copilot account lists no chat model it may use".into());
    }
    if let Some(model) = selected {
        return if account.get(model).is_some() {
            Ok(None)
        } else {
            Err(format!(
                "the model '{model}' is not one this Copilot account may use; allowed: {}. \
                 Select one with --model <id> in the copilot seat's headless_invocation \
                 (clis.toml)",
                account.ids()
            ))
        };
    }
    let cheapest = account
        .models
        .iter()
        .filter(|m| m.multiplier.is_some() || m.price.is_some())
        .min_by_key(|m| {
            (
                m.multiplier.unwrap_or(u64::MAX),
                m.price.unwrap_or(u64::MAX),
            )
        })
        .map(|m| m.id.as_str());
    let small = SMALL_MODEL_MARKERS.iter().find_map(|marker| {
        account
            .models
            .iter()
            .find(|m| m.id.contains(marker))
            .map(|m| m.id.as_str())
    });
    let pick = PREFERRED_MODELS
        .iter()
        .copied()
        .find(|p| account.get(p).is_some())
        .or(cheapest)
        .or_else(|| {
            account
                .default
                .as_deref()
                .filter(|d| account.get(d).is_some())
        })
        .or(small)
        .unwrap_or(&account.models[0].id);
    Ok(Some(pick.to_string()))
}

/// The wire API a copilot seat uses for `model` (pure): the name rule
/// ([`crate::seat_gateway::copilot_wire_api`]: `gpt-*` → responses, else the default
/// completions), unless the account says the model is not served there and is served on the
/// other one. `Some("responses")` / `Some("completions")` = set it; `None` = leave the default.
pub(crate) fn wire_api(
    args: &[String],
    env: impl Fn(&str) -> Option<String>,
    account: Option<&CopilotAccount>,
) -> Option<&'static str> {
    if env("COPILOT_PROVIDER_WIRE_API").is_some_and(|v| !v.is_empty()) {
        return None;
    }
    let by_name = crate::seat_gateway::copilot_wire_api(args, &env);
    let served = selected_model(args, &env)
        .and_then(|m| account.and_then(|a| a.get(&m)).cloned())
        .filter(|m| !m.endpoints.is_empty());
    let Some(served) = served else {
        return by_name;
    };
    let on = |p: &str| served.endpoints.iter().any(|e| e == p);
    match by_name {
        Some("responses") if !on("/responses") && on("/chat/completions") => Some("completions"),
        None if !on("/chat/completions") && on("/responses") => Some("responses"),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn argv(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    /// A `/models` answer in the Copilot API's shape (synthetic).
    fn list() -> Value {
        json!({"object": "list", "data": [
            {"id": "gpt-5-mini", "capabilities": {"type": "chat"},
             "policy": {"state": "unconfigured"}, "model_picker_enabled": true},
            {"id": "gpt-4.1", "capabilities": {"type": "chat"}, "policy": {"state": "enabled"},
             "model_picker_enabled": true, "is_chat_default": true,
             "supported_endpoints": ["/chat/completions", "/responses"]},
            {"id": "claude-sonnet-4.5", "capabilities": {"type": "chat"},
             "model_picker_enabled": true, "supported_endpoints": ["/chat/completions", "/v1/messages"],
             "billing": {"multiplier": 1, "token_prices": {"batch_size": 1000000,
                "default": {"input_price": 3, "output_price": 15}}}},
            {"id": "gpt-5.5", "capabilities": {"type": "chat"}, "model_picker_enabled": true,
             "supported_endpoints": ["/responses"]},
            {"id": "gpt-4o-2024-05-13", "capabilities": {"type": "chat"},
             "model_picker_enabled": false},
            {"id": "o3-mini", "capabilities": {"type": "chat"}, "policy": {"state": "disabled"}},
            {"id": "text-embedding-3-small", "capabilities": {"type": "embeddings"}},
            {"id": "has space", "capabilities": {"type": "chat"}},
            {"id": "gpt-4.1", "capabilities": {"type": "chat"}}
        ]})
    }

    #[test]
    fn the_account_host_is_its_endpoints_api_and_nothing_else() {
        let user = |api: &str| json!({"login": "fake", "endpoints": {"api": api}});
        assert_eq!(
            api_host(&user("https://api.individual.githubcopilot.com")).unwrap(),
            "api.individual.githubcopilot.com"
        );
        assert_eq!(
            api_host(&user("https://api.business.githubcopilot.com/")).unwrap(),
            "api.business.githubcopilot.com"
        );
        for bad in [
            "http://api.individual.githubcopilot.com",
            "https://evil.example.com",
            "https://api.githubcopilot.com.evil.example.com",
            "https://api.individual.githubcopilot.com:8443",
            "",
        ] {
            assert!(api_host(&user(bad)).is_err(), "{bad}");
        }
        assert!(api_host(&json!({"login": "fake"})).is_err());
    }

    #[test]
    fn only_models_the_account_may_run_are_kept() {
        let a = account_models(&list(), 7).unwrap();
        let ids: Vec<&str> = a.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["gpt-4.1", "claude-sonnet-4.5", "gpt-5.5"]);
        assert_eq!(a.default.as_deref(), Some("gpt-4.1"));
        assert_eq!(a.resolved_at, 7);
        assert_eq!(a.models[0].endpoints, ["/chat/completions", "/responses"]);
        assert_eq!((a.models[0].multiplier, a.models[0].price), (None, None));
        assert_eq!(
            (a.models[1].multiplier, a.models[1].price),
            (Some(1000), Some(18))
        );
        assert!(account_models(&json!({"error": "x"}), 7).is_err());
        // The broker files round-trip; the host is validated again on the way in.
        let files = account_files("api.individual.githubcopilot.com", &a).unwrap();
        assert_eq!(
            files[0],
            (
                HOST_FILE.into(),
                b"api.individual.githubcopilot.com".to_vec()
            )
        );
        assert_eq!(parse_models_file(&files[1].1), Some(a.clone()));
        assert!(account_files("evil.example.com", &a).is_err());
        assert_eq!(parse_models_file(b"not json"), None);
    }

    #[test]
    fn a_seat_runs_an_allowed_model_or_is_refused_by_name() {
        let a = account_models(&list(), 7).unwrap();
        // Nothing selects one: the first preferred model the account allows (gpt-5-mini is
        // unconfigured here, so gpt-4.1).
        assert_eq!(
            seat_model(None, Some(&a)).unwrap().as_deref(),
            Some("gpt-4.1")
        );
        // An allowed selection is kept as it is.
        assert_eq!(
            seat_model(Some("claude-sonnet-4.5"), Some(&a)).unwrap(),
            None
        );
        // A selection the account may not use is refused, naming the allowed models.
        let why = seat_model(Some("gpt-5-mini"), Some(&a)).unwrap_err();
        assert!(why.contains("'gpt-5-mini'"), "{why}");
        assert!(why.contains("gpt-4.1, claude-sonnet-4.5, gpt-5.5"), "{why}");
        assert!(why.contains("clis.toml"), "{why}");
        // No preferred model: the account's own default, else its first.
        let only = |ids: &[&str], default: Option<&str>| CopilotAccount {
            models: ids
                .iter()
                .map(|i| CopilotModel {
                    id: i.to_string(),
                    endpoints: vec![],
                    multiplier: None,
                    price: None,
                })
                .collect(),
            default: default.map(str::to_string),
            resolved_at: 1,
        };
        assert_eq!(
            seat_model(None, Some(&only(&["a", "b"], Some("b"))))
                .unwrap()
                .as_deref(),
            Some("b")
        );
        assert_eq!(
            seat_model(None, Some(&only(&["a", "b"], None)))
                .unwrap()
                .as_deref(),
            Some("a")
        );
        assert!(seat_model(None, Some(&only(&[], None))).is_err());
        // No preferred model but billing data: the cheapest model the account may use, by its
        // premium multiplier, then its token price; the account's default comes after.
        let mut priced = only(&["opus", "mini", "haiku", "free"], Some("opus"));
        priced.models[0].multiplier = Some(3000);
        priced.models[1].multiplier = Some(330);
        priced.models[1].price = Some(9);
        priced.models[2].multiplier = Some(330);
        priced.models[2].price = Some(5);
        assert_eq!(
            seat_model(None, Some(&priced)).unwrap().as_deref(),
            Some("haiku")
        );
        priced.models[3].multiplier = Some(0);
        assert_eq!(
            seat_model(None, Some(&priced)).unwrap().as_deref(),
            Some("free")
        );
        // No preferred model and no billing: the account's own default first; with none (the
        // operator's enterprise list), the small tier by marker order, before the first model.
        let ids = [
            "claude-fable-5.1",
            "claude-haiku-5.5",
            "gemini-3.8-flash",
            "gpt-5.4-mini",
        ];
        assert_eq!(
            seat_model(None, Some(&only(&ids, Some("claude-fable-5.1"))))
                .unwrap()
                .as_deref(),
            Some("claude-fable-5.1")
        );
        assert_eq!(
            seat_model(None, Some(&only(&ids, None)))
                .unwrap()
                .as_deref(),
            Some("gpt-5.4-mini")
        );
        let big = only(&["opus-x", "sonnet-y"], Some("sonnet-y"));
        assert_eq!(
            seat_model(None, Some(&big)).unwrap().as_deref(),
            Some("sonnet-y")
        );
        // Unresolved: refused with the reason, never run on a guess.
        assert!(seat_model(None, None)
            .unwrap_err()
            .contains("no resolved Copilot account"));
    }

    #[test]
    fn the_selected_model_follows_copilots_precedence() {
        let none = |_: &str| None::<String>;
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert_eq!(selected_model(&argv(&["copilot", "-p", "x"]), none), None);
        assert_eq!(
            selected_model(&argv(&["copilot", "--model", "a", "--model=b"]), none).as_deref(),
            Some("b")
        );
        assert_eq!(
            selected_model(&argv(&["copilot", "--", "--model", "a"]), none),
            None
        );
        assert_eq!(
            selected_model(
                &argv(&["copilot", "--model", "a"]),
                env(&[("COPILOT_PROVIDER_WIRE_MODEL", "w"), ("COPILOT_MODEL", "m")])
            )
            .as_deref(),
            Some("w")
        );
        assert_eq!(
            selected_model(
                &argv(&["copilot"]),
                env(&[("COPILOT_PROVIDER_MODEL_ID", "p")])
            )
            .as_deref(),
            Some("p")
        );
        assert_eq!(
            selected_model(&argv(&["copilot"]), env(&[("COPILOT_MODEL", "")])),
            None
        );
    }

    #[test]
    fn the_wire_api_follows_where_the_account_serves_the_model() {
        let a = account_models(&list(), 7).unwrap();
        let none = |_: &str| None::<String>;
        let wire = |m: &str| wire_api(&argv(&["copilot", "--model", m]), none, Some(&a));
        assert_eq!(
            wire("gpt-4.1"),
            Some("responses"),
            "both served: the name rule"
        );
        assert_eq!(wire("gpt-5.5"), Some("responses"));
        assert_eq!(wire("claude-sonnet-4.5"), None, "completions, the default");
        let mut chat_only = a.clone();
        chat_only.models[0].endpoints = vec!["/chat/completions".into()];
        assert_eq!(
            wire_api(
                &argv(&["copilot", "--model", "gpt-4.1"]),
                none,
                Some(&chat_only)
            ),
            Some("completions")
        );
        let mut responses_only = a.clone();
        responses_only.models[1].endpoints = vec!["/responses".into()];
        assert_eq!(
            wire_api(
                &argv(&["copilot", "--model", "claude-sonnet-4.5"]),
                none,
                Some(&responses_only)
            ),
            Some("responses")
        );
        // Unknown endpoints or no account: the name rule; an explicit wire API is never touched.
        assert_eq!(
            wire_api(&argv(&["copilot", "--model", "gpt-9"]), none, None),
            Some("responses")
        );
        let explicit = |k: &str| (k == "COPILOT_PROVIDER_WIRE_API").then(|| "completions".into());
        assert_eq!(
            wire_api(
                &argv(&["copilot", "--model", "gpt-5.5"]),
                explicit,
                Some(&a)
            ),
            None
        );
    }
}
