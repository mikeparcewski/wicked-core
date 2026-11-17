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

/// The carrier's half: record what a launch it ran on this thread was observed calling.
pub(crate) fn record_receipt(seat: &str, providers: Option<Vec<Provider>>) {
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
    /// validator author's re-admission, crew#972 decision 2, builds on it).
    #[cfg_attr(not(test), allow(dead_code))]
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

/// Whether a judge's verdict stands against what the work was observed calling: a judge seen
/// calling a provider the work was seen calling is not independent of it, and the reason is
/// returned. A KNOWN shared provider is proof even when other turns went unobserved; with no
/// shared provider seen nothing is proven here (`None`) and the caller's own rules decide —
/// nothing is ever re-admitted on an incomplete observation ([`Observed::known`]).
pub(crate) fn same_provider(judge: &Observed, work: &Observed) -> Option<String> {
    let shared: Vec<&str> = judge
        .providers
        .iter()
        .filter(|p| work.providers.contains(p))
        .map(String::as_str)
        .collect();
    (!shared.is_empty()).then(|| {
        format!(
            "the judge called the same provider as the work it judged ({}), as the seat gateway \
             recorded both: its approval is not independent and is discarded",
            shared.join(", ")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seat_gateway::GatewayRecord;

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
        let seen = |p: &[&str], complete: bool| Observed {
            providers: p.iter().map(|s| s.to_string()).collect(),
            complete,
        };
        assert!(same_provider(&seen(&["a"], true), &seen(&["a"], true))
            .is_some_and(|w| w.contains("same provider")));
        assert!(
            same_provider(&seen(&["a"], true), &m).is_some(),
            "proof despite a gap"
        );
        assert_eq!(
            same_provider(&seen(&["a"], true), &seen(&["c"], true)),
            None
        );
        assert_eq!(
            same_provider(&Observed::default(), &seen(&["c"], true)),
            None
        );
    }
}
