//! (core#1543) Seat identity OBSERVED: the provider each launch actually called, as the seat
//! gateway recorded it — never predicted from a seat key, an invocation, a path or a binary.
//!
//! On the container floor every model request a seat makes is authenticated at the daemon's
//! gateway with the unit key the engine minted for that one launch, and each route admits only
//! its owner seat's keys. The access log names the upstream the gateway sent each request to
//! (`endpoint`, from the gateway's own route config). So a launch's observed PROVIDERS are the
//! upstreams of its successful, key-attributed requests. A launch with none — the host floor, no
//! gateway lease, a log the engine could not read, a CLI that never reached its route — is
//! UNOBSERVED: it proves nothing, and nothing is ever re-admitted on it.

/// An observed provider: the upstream endpoint the gateway sent a launch's requests to.
pub(crate) type Provider = String;

/// The providers `records` (one launch's gateway records) show it calling: the endpoints of its
/// successful requests that the gateway forwarded upstream (readiness hits and refusals carry
/// none). `None`: no such request, so the launch is unobserved.
pub(crate) fn providers_of(
    records: &[crate::seat_gateway::GatewayRecord],
) -> Option<Vec<Provider>> {
    let mut out: Vec<Provider> = Vec::new();
    for r in records.iter().filter(|r| (200..300).contains(&r.status)) {
        if let Some(e) = r.endpoint.as_deref().filter(|e| !e.trim().is_empty()) {
            if !out.iter().any(|o| o == e) {
                out.push(e.to_string());
            }
        }
    }
    out.sort();
    (!out.is_empty()).then_some(out)
}

/// What one launch was observed calling: the seat it was launched as and its providers (`None`:
/// unobserved).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LaunchReceipt {
    pub(crate) seat: String,
    pub(crate) providers: Option<Vec<Provider>>,
}

thread_local! {
    /// The receipts of the launches this thread made, in order: the carrier runs each launch on
    /// its caller's thread, so the engine reads back exactly the launches it asked for
    /// ([`take_receipts`]). A launch no carrier recorded is unobserved.
    static RECEIPTS: std::cell::RefCell<Vec<LaunchReceipt>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

thread_local! {
    /// (coord codex-check on #1547) The providers the work being judged on this thread was seen
    /// calling, while its judge search runs ([`JudgingWork`]).
    static AVOID: std::cell::RefCell<Judging> = const {
        std::cell::RefCell::new(Judging { avoid: Vec::new(), require_observed: false })
    };
}

/// The providers each seat's latest observed launch called, process-wide: the judge rotation
/// tries first the seats not known to share the work's provider.
static LAST_SEEN: std::sync::Mutex<Option<std::collections::HashMap<String, Vec<Provider>>>> =
    std::sync::Mutex::new(None);

/// What the judge search on this thread must be independent of ([`JudgingWork`]).
#[derive(Debug, Default)]
struct Judging {
    avoid: Vec<Provider>,
    /// (crew#972 decision 2) The pinned validator's author was re-admitted: an approval stands
    /// only from a launch observed, fully, calling none of `avoid`.
    require_observed: bool,
}

/// While held, the judge rotations on this thread judge work known to have called `work`'s
/// providers: they try first the seats not known to share them, and pass over an APPROVAL from a
/// seat seen calling one of them to the next seat ([`approval_is_independent`]). Restored on drop.
pub(crate) struct JudgingWork(Judging);

impl JudgingWork {
    pub(crate) fn of(work: &Observed) -> Self {
        let now = Judging {
            avoid: work.providers.clone(),
            require_observed: false,
        };
        Self(AVOID.with(|a| std::mem::replace(&mut *a.borrow_mut(), now)))
    }

    /// (crew#972 decision 2) From here on, the search takes an approval only from a judge whose
    /// own launch was observed, fully, calling another provider than the work's: the validator
    /// author's re-admission rests on observation, never on trust.
    pub(crate) fn require_observed(&self) {
        AVOID.with(|a| a.borrow_mut().require_observed = true);
    }
}

impl Drop for JudgingWork {
    fn drop(&mut self) {
        let prev = std::mem::take(&mut self.0);
        AVOID.with(|a| *a.borrow_mut() = prev);
    }
}

/// Whether `seat` is KNOWN (by its latest observed launch anywhere in this process) to call a
/// provider the work being judged on this thread called: tried last, never skipped on a guess.
pub(crate) fn known_to_share(seat: &str) -> bool {
    let avoid = AVOID.with(|a| a.borrow().avoid.clone());
    shared_with(seat, &avoid).is_some()
}

/// The providers `seat`'s latest observed launch in this process called (`None`: never observed).
pub(crate) fn last_seen(seat: &str) -> Option<Vec<Provider>> {
    let seen = LAST_SEEN.lock().unwrap_or_else(|p| p.into_inner());
    seen.as_ref().and_then(|m| m.get(seat)).cloned()
}

/// THE "known to share" rule ([`known_to_share`] for the judge rotation, and the launch's
/// [`crate::judge_coverage`]): the first of `avoid` that `seat`'s latest observed launch called.
/// `None` when the seat was never observed or called none of them: never a guess.
pub(crate) fn shared_with(seat: &str, avoid: &[Provider]) -> Option<Provider> {
    if avoid.is_empty() {
        return None;
    }
    last_seen(seat)?.into_iter().find(|p| avoid.contains(p))
}

/// (core#1540) One seat identity: its key, then this attempt's providers or LAST_SEEN.
pub(crate) fn shares_identity(
    seat: &str,
    own_observed: Option<&[Provider]>,
    author_keys: &[String],
    author_providers: &[Provider],
) -> Option<String> {
    if author_keys
        .iter()
        .any(|a| a == seat || a == crate::team::runner::seat_key(seat))
    {
        return Some(format!("seat `{seat}` authored the work"));
    }
    let shared = match own_observed {
        Some(ps) => ps.iter().find(|p| author_providers.contains(p)).cloned(),
        None => shared_with(seat, author_providers),
    };
    shared.map(|p| {
        if author_keys.is_empty() {
            return format!("observed calling `{p}`, a provider of the work");
        }
        format!(
            "observed calling `{p}`, the provider the work's author {} called",
            author_keys
                .iter()
                .map(|a| format!("`{a}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

/// The providers of the work being judged on this thread ([`JudgingWork`]); empty when none is held.
pub(crate) fn work_providers() -> Vec<Provider> {
    AVOID.with(|a| a.borrow().avoid.clone())
}

/// (core#1546) Bus launches have no receipts and cannot satisfy this requirement.
pub(crate) fn observation_required() -> bool {
    AVOID.with(|a| a.borrow().require_observed)
}

/// What `seat`'s launches on this thread were observed calling (empty: nothing observed).
pub(crate) fn observed_this_thread(seat: &str) -> Vec<Provider> {
    RECEIPTS.with(|r| observed_for(&r.borrow(), seat).providers)
}

/// Whether an approval `seat` just gave on this thread stands as independent of the work being
/// judged: `false` when this thread's receipts show its launch calling one of the work's providers.
/// Unobserved: `true` (nothing proven; the caller's own rules stand) — unless the search
/// [requires observation](JudgingWork::require_observed), when it is `false`.
pub(crate) fn approval_is_independent(seat: &str) -> bool {
    let (avoid, require_observed) =
        AVOID.with(|a| (a.borrow().avoid.clone(), a.borrow().require_observed));
    let mine = RECEIPTS.with(|r| observed_for(&r.borrow(), seat));
    if require_observed && mine.known().is_none() {
        return false;
    }
    !mine.providers.iter().any(|p| avoid.contains(p))
}

/// The carrier's half: record what a launch it ran on this thread was observed calling.
pub(crate) fn record_receipt(seat: &str, providers: Option<Vec<Provider>>) {
    if let Some(ps) = providers.as_ref().filter(|p| !p.is_empty()) {
        LAST_SEEN
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_or_insert_with(std::collections::HashMap::new)
            .insert(seat.to_string(), ps.clone());
    }
    RECEIPTS.with(|r| {
        r.borrow_mut().push(LaunchReceipt {
            seat: seat.to_string(),
            providers,
        })
    });
}

/// The engine's half: every receipt recorded on this thread since the last take.
pub(crate) fn take_receipts() -> Vec<LaunchReceipt> {
    RECEIPTS.with(|r| std::mem::take(&mut *r.borrow_mut()))
}

/// What the launches of one seat among a set of receipts were observed calling: every provider
/// any of them was seen calling, and whether EVERY one of them was observed. A known shared
/// provider is proven by `providers` alone; proving that two launches are independent also needs
/// `complete` (one unobserved turn may have called anything).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Observed {
    pub(crate) providers: Vec<Provider>,
    pub(crate) complete: bool,
}

impl Observed {
    /// Fully observed, with at least one provider: what independence can be proven on (the
    /// validator author's re-admission, crew#972 decision 2).
    pub(crate) fn known(&self) -> Option<&[Provider]> {
        (self.complete && !self.providers.is_empty()).then_some(self.providers.as_slice())
    }
}

/// [`Observed`] for the launches of `seat` among `receipts`; never launched: nothing known, not
/// complete.
pub(crate) fn observed_for(receipts: &[LaunchReceipt], seat: &str) -> Observed {
    let mut out = Observed::default();
    let mut any = false;
    let mut all = true;
    for r in receipts.iter().filter(|r| r.seat == seat) {
        any = true;
        match &r.providers {
            Some(ps) => {
                for p in ps {
                    if !out.providers.contains(p) {
                        out.providers.push(p.clone());
                    }
                }
            }
            None => all = false,
        }
    }
    out.providers.sort();
    out.complete = any && all;
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seat_gateway::GatewayRecord;

    #[test]
    fn identity_uses_the_attempt_before_last_seen_and_always_the_seat_key() {
        let authors = vec!["identity-author".into()];
        let providers = vec!["identity-provider:443".into()];
        record_receipt("identity-reviewer", Some(providers.clone()));
        assert!(shares_identity("identity-reviewer", None, &authors, &providers).is_some());
        assert!(shares_identity("identity-reviewer", Some(&[]), &authors, &providers).is_none());
        assert!(shares_identity("identity-cold", Some(&providers), &authors, &providers).is_some());
        assert!(shares_identity("identity-author#2", Some(&[]), &authors, &[]).is_some());
        assert!(shares_identity("identity-unseen", None, &authors, &providers).is_none());
        take_receipts();
    }

    fn rec(status: u16, endpoint: Option<&str>) -> GatewayRecord {
        GatewayRecord {
            status,
            unit: "u".into(),
            run: "r".into(),
            endpoint: endpoint.map(str::to_string),
            model: Some("whatever-the-seat-claims".into()),
            tools: Vec::new(),
            input_tokens: None,
            output_tokens: None,
            text: Vec::new(),
        }
    }

    /// (KEY, SILENT-OK) Only a successful request the gateway forwarded upstream counts, by the
    /// upstream it went to — never the model the seat claimed; nothing forwarded is unobserved.
    #[test]
    fn a_launch_is_the_upstream_its_requests_went_to() {
        assert_eq!(
            providers_of(&[
                rec(200, Some("chatgpt.com:443")),
                rec(200, Some("chatgpt.com:443")),
                rec(200, None),          // the gateway's own readiness answer
                rec(401, Some("x:443")), // refused
                rec(500, Some("y:443")), // failed upstream: not proof it was served
            ]),
            Some(vec!["chatgpt.com:443".to_string()])
        );
        assert_eq!(providers_of(&[rec(200, None), rec(401, Some("x"))]), None);
        assert_eq!(providers_of(&[]), None);
    }

    /// The real access log's shape: `endpoint` is kept, gateway-side.
    #[test]
    fn the_real_access_log_names_the_upstream() {
        let log = include_str!("../tests/fixtures/gateway/access-log.ndjson");
        let r = crate::seat_gateway::parse_records(log, "1");
        assert_eq!(providers_of(&r), Some(vec!["gw1-stub:8080".to_string()]));
    }

    /// Receipts, by seat: what was seen is kept (proof of a shared provider) and an unobserved turn
    /// makes the set incomplete (never proof of independence) — codex review on P1.
    #[test]
    fn receipts_and_the_shared_provider_rule() {
        take_receipts();
        record_receipt("codex", Some(vec!["chatgpt.com:443".into()]));
        record_receipt("claude", Some(vec!["api.anthropic.com:443".into()]));
        let r = take_receipts();
        assert!(take_receipts().is_empty(), "taken once");
        let codex = observed_for(&r, "codex");
        assert_eq!(codex.known(), Some(&["chatgpt.com:443".to_string()][..]));
        assert_eq!(
            observed_for(&r, "pi").known(),
            None,
            "never launched: unobserved"
        );
        let mixed = vec![
            LaunchReceipt {
                seat: "codex".into(),
                providers: None,
            },
            LaunchReceipt {
                seat: "codex".into(),
                providers: Some(vec!["a".into()]),
            },
        ];
        let m = observed_for(&mixed, "codex");
        assert_eq!(m.providers, vec!["a".to_string()]);
        assert!(!m.complete && m.known().is_none());
        // The judge's search: an approval from a seat seen calling the work's provider is not
        // independent; another provider, or an unobserved launch, proves nothing against it.
        let work = Observed {
            providers: vec!["chatgpt.com:443".into()],
            complete: true,
        };
        let _judging = JudgingWork::of(&work);
        take_receipts();
        record_receipt("pi", Some(vec!["chatgpt.com:443".into()]));
        record_receipt("claude", Some(vec!["api.anthropic.com:443".into()]));
        record_receipt("agy", None);
        assert!(!approval_is_independent("pi"));
        assert!(approval_is_independent("claude"));
        assert!(approval_is_independent("agy"));
        // (crew#972 decision 2) After a re-admission the search requires observation: an
        // unobserved or partly observed launch is never independent; a fully observed one on
        // another provider is.
        _judging.require_observed();
        record_receipt("opencode", Some(vec!["api.anthropic.com:443".into()]));
        record_receipt("opencode", None);
        assert!(approval_is_independent("claude"));
        assert!(!approval_is_independent("agy"), "unobserved");
        assert!(!approval_is_independent("opencode"), "partly observed");
        assert!(!approval_is_independent("pi"));
        drop(_judging);
        assert!(
            approval_is_independent("pi") && approval_is_independent("agy"),
            "outside a search nothing is avoided or required"
        );
        take_receipts();
    }
}
