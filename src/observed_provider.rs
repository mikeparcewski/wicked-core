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

/// Everything the launches of `seat` among `receipts` were observed calling; `None` when there was
/// none, or any of them was unobserved (one unobserved turn hides what it called).
pub(crate) fn observed_for(receipts: &[LaunchReceipt], seat: &str) -> Option<Vec<Provider>> {
    let mut out: Vec<Provider> = Vec::new();
    let mut any = false;
    for r in receipts.iter().filter(|r| r.seat == seat) {
        any = true;
        for p in r.providers.as_ref()? {
            if !out.contains(p) {
                out.push(p.clone());
            }
        }
    }
    any.then_some(out)
}

/// Whether a judge's verdict stands against what the work was observed calling: a judge observed
/// calling a provider the work was observed calling is not independent of it — its verdict is
/// discarded and the reason returned. When either side is unobserved nothing is proven here
/// (`None`); the caller's own rules decide, and nothing is re-admitted on an unobserved launch.
pub(crate) fn same_provider(
    judge: Option<&[Provider]>,
    work: Option<&[Provider]>,
) -> Option<String> {
    let (judge, work) = (judge?, work?);
    let shared: Vec<&str> = judge
        .iter()
        .filter(|p| work.contains(p))
        .map(String::as_str)
        .collect();
    (!shared.is_empty()).then(|| {
        format!(
            "the judge called the same provider as the work it judged ({}), as the seat gateway \
             recorded both: its verdict is discarded",
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

    /// Receipts: by seat, any unobserved turn makes the seat's launches unobserved; the verdict
    /// check proves only what both sides show.
    #[test]
    fn receipts_and_the_shared_provider_rule() {
        take_receipts();
        record_receipt("codex", Some(vec!["chatgpt.com:443".into()]));
        record_receipt("claude", Some(vec!["api.anthropic.com:443".into()]));
        let r = take_receipts();
        assert!(take_receipts().is_empty(), "taken once");
        assert_eq!(
            observed_for(&r, "codex"),
            Some(vec!["chatgpt.com:443".to_string()])
        );
        assert_eq!(observed_for(&r, "pi"), None, "never launched: unobserved");
        let mixed = vec![
            LaunchReceipt {
                seat: "codex".into(),
                providers: Some(vec!["a".into()]),
            },
            LaunchReceipt {
                seat: "codex".into(),
                providers: None,
            },
        ];
        assert_eq!(
            observed_for(&mixed, "codex"),
            None,
            "one unobserved turn hides the rest"
        );
        let a = vec!["api.anthropic.com:443".to_string()];
        let c = vec!["chatgpt.com:443".to_string()];
        assert!(same_provider(Some(&a), Some(&a)).is_some_and(|w| w.contains("same provider")));
        assert_eq!(same_provider(Some(&a), Some(&c)), None);
        assert_eq!(same_provider(None, Some(&c)), None);
        assert_eq!(same_provider(Some(&a), None), None);
    }
}
