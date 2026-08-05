//! Rendering an error so the *reason* survives to whoever has to act on it.
//!
//! # The defect this exists to prevent
//!
//! `anyhow::Error`'s `Display` prints **only the outermost context**. The chain that explains what
//! actually went wrong is reachable, but `{e}` does not reach it — `{e:#}` does. Measured:
//!
//! ```text
//! {e}    -> parsing workflow file /tmp/x.json
//! {e:#}  -> parsing workflow file /tmp/x.json: EOF while parsing an object at line 1 column 1
//! ```
//!
//! That is not cosmetic. Two places were losing the only useful half of the message:
//!
//! 1. **The workflow loader.** A def core cannot deserialise is skipped, named — and unexplained.
//!    Observed verbatim in a live daemon log, the path printed twice and no cause at all:
//!    `skipping workflow file …/probe-002-b.json (parsing workflow file …/probe-002-b.json)`.
//!    The API had rejected that same def with `unknown field 'name' at line 1 column 26`; the
//!    loader threw it away. An operator whose workflow vanished gets the path they already knew.
//!
//! 2. **Governance infra denials.** `append_infra_deny` writes the reason into a durable
//!    `ConformanceClaim` (`obligations` + `criteria`). A DENY whose recorded cause is
//!    `store open failed` — with the path, the errno and the backend it failed on discarded — is
//!    a permanent record that cannot be acted on later.
//!
//! This is FINDING-050's family: *a denial that does not say what it denied is a second defect*.
//! The remedy there was to distinguish three collapsed causes; the remedy here is to stop
//! truncating the one we already have.
//!
//! # Why a function and not `{e:#}` inline
//!
//! `{e:#}` is one character and easy to write; it is also one character and easy to omit, and
//! omitting it fails **silently and legibly** — the message still looks like a real message. A
//! named call is greppable, its intent is on its face, and the behaviour it guarantees can be
//! tested once rather than re-asserted at each site.
//!
//! Taking `&anyhow::Error` (not `impl Display`) is deliberate: it makes the compiler reject sites
//! where the alternate form would be a no-op, so this cannot spread to `io::Error` call sites and
//! imply a guarantee it is not adding there.

/// `label`, then the error's full cause chain.
///
/// Use for any message a human or a durable record will read: log lines, denial reasons,
/// terminal-state causes.
pub(crate) fn with_cause(label: &str, e: &anyhow::Error) -> String {
    // `{:#}` is anyhow's alternate Display: every context layer, outermost first, `: `-joined.
    format!("{label}: {e:#}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    /// The error the workflow loader actually gets: a real serde rejection of a real unknown
    /// field, wrapped in the same `.context(...)` layer `def_from_file` adds.
    fn parse_failure() -> anyhow::Error {
        #[derive(serde::Deserialize, Debug)]
        #[serde(deny_unknown_fields)]
        #[allow(dead_code)]
        struct Def {
            id: String,
        }
        serde_json::from_str::<Def>(r#"{"id":"x","name":"y"}"#)
            .context("parsing workflow file /tmp/x.json")
            .expect_err("an unknown field must be rejected")
    }

    /// THE invariant. Falsifier: render with `{e}` instead of `{e:#}` and this fails — the
    /// serde detail is exactly what the live daemon log was missing.
    #[test]
    fn the_underlying_reason_survives_rendering() {
        let msg = with_cause("skipping workflow file /tmp/x.json", &parse_failure());
        assert!(
            msg.contains("unknown field"),
            "the cause chain was truncated — this is the defect: {msg}"
        );
    }

    /// The label must still be there: the cause alone does not say which file or which operation.
    #[test]
    fn the_label_is_kept_alongside_the_cause() {
        let msg = with_cause("store open failed", &parse_failure());
        assert!(msg.starts_with("store open failed: "), "{msg}");
    }

    /// A context-free error must not render as an empty or dangling reason.
    #[test]
    fn an_error_with_no_context_still_renders_its_own_message() {
        let msg = with_cause("policy select failed", &anyhow::anyhow!("db is locked"));
        assert_eq!(msg, "policy select failed: db is locked");
    }
}
