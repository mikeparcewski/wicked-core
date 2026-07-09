//! VALIDATOR VAULT — the durable, content-addressed store for APPROVED deterministic validators
//! (DES-EXEC-001 rev0.4, "pin + vault"). Once a human/council has approved an authored
//! [`DeterministicValidator`], it is PINNED — assigned a deterministic content-hash id derived from its
//! `criterion`, `script`, and `approved` flag — and VAULTED as a graph node keyed by that pin. The pin is
//! the stable, tamper-evident handle a phase carries: because the id is a hash of the validator's bytes,
//! a pin can only ever load back the exact validator that produced it. Change the script and the pin
//! changes, so a swapped-out script can never masquerade under an old, already-approved pin.
//!
//! This follows the same estate-node persistence pattern as [`crate::domain`]: the validator serializes
//! into one `Node.metadata` object (serde) and round-trips losslessly, so the `approved` flag — the gate
//! authorization the rest of rev0.4 leans on — is preserved faithfully. [`load_validator`] returns
//! EXACTLY what [`store_validator`] wrote; it does not re-approve, downgrade, or otherwise mutate the
//! validator. Whether a loaded validator is gate-ready is therefore decided by its own `approved` flag,
//! which [`crate::validator::run_validator`] still checks fail-closed before any execution.

use wicked_apps_core::{
    synthetic_symbol, GraphRead, Language, Location, Node, NodeKind, Span, SqliteStore,
    SYMBOL_SCHEME,
};

use crate::domain::put_node;
use crate::validator::{author_deterministic_validator, DeterministicValidator};
use crate::workflow::StepRunner;

/// The estate node-kind tag under which vaulted validators are persisted (mirrors
/// [`crate::execute::WORK_OUTPUT`] and the domain kinds — a stable string used both as the
/// `NodeKind::Other` discriminant and the [`synthetic_symbol`] scheme prefix).
pub const VALIDATOR_VAULT: &str = "validator_vault";

/// PIN a validator: a deterministic content-hash id (a 16-hex-char sha256 prefix) over its `criterion`,
/// `script`, and `approved` flag. Same validator ⇒ same pin (content addressing); any change to the
/// script (or criterion, or the approval flag) yields a different pin. This is the "pin" of rev0.4's
/// "pin + vault": a stable handle a phase can carry that can only ever resolve back to the exact approved
/// validator bytes it was minted from. NUL separators between fields keep the hash unambiguous, so e.g.
/// `("ab", "")` and `("a", "b")` never collide onto the same pin.
#[must_use]
pub fn pin(v: &DeterministicValidator) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(v.criterion.as_bytes());
    hasher.update([0u8]);
    hasher.update(v.script.as_bytes());
    hasher.update([0u8]);
    hasher.update([u8::from(v.approved)]);
    format!("{:x}", hasher.finalize())[..16].to_string()
}

/// Serialize `v` into a vault node (keyed by its [`pin`]) exactly as the domain objects persist — the
/// whole validator becomes one `Node.metadata` JSON object, so every field (including `approved`)
/// round-trips losslessly through [`load_validator`].
fn to_vault_node(v: &DeterministicValidator) -> Node {
    let id = pin(v);
    let mut node = Node::new(
        synthetic_symbol(VALIDATOR_VAULT, &id),
        NodeKind::Other(VALIDATOR_VAULT.to_string()),
        id.clone(),
        Language::new(SYMBOL_SCHEME),
        Location::new(format!("{VALIDATOR_VAULT}/{id}"), Span::ZERO),
    );
    if let serde_json::Value::Object(map) =
        serde_json::to_value(v).expect("DeterministicValidator serializes to JSON")
    {
        node.metadata = map;
    }
    node
}

/// VAULT a validator: persist it as a graph node keyed by its [`pin`] and return that pin. Idempotent —
/// because the key IS the content hash, re-storing the same validator upserts the identical node and
/// yields the same pin. Storing does NOT approve: the `approved` flag is written faithfully, so an
/// unapproved validator vaults as unapproved (and will still be refused fail-closed at run time). Runs on
/// the actor (single-writer) thread via [`put_node`].
pub fn store_validator(
    store: &mut SqliteStore,
    v: &DeterministicValidator,
) -> anyhow::Result<String> {
    let id = pin(v);
    put_node(store, to_vault_node(v))?;
    Ok(id)
}

/// Load a vaulted validator by its [`pin`]. Returns `Ok(None)` for an unknown pin (never an error), and
/// otherwise deserializes back EXACTLY what [`store_validator`] wrote — including the `approved` flag.
/// This is a pure read: it does not re-approve or downgrade the validator. Whether the result is
/// gate-ready is decided by its own `approved` flag, which [`crate::validator::run_validator`] checks
/// fail-closed before it will execute.
pub fn load_validator(
    store: &SqliteStore,
    pin: &str,
) -> anyhow::Result<Option<DeterministicValidator>> {
    match store.get_node(&synthetic_symbol(VALIDATOR_VAULT, pin))? {
        Some(node) => {
            let v: DeterministicValidator = serde_json::from_value(serde_json::Value::Object(
                node.metadata.clone(),
            ))
            .map_err(|e| {
                anyhow::anyhow!(
                    "vault node {} is not a valid DeterministicValidator: {e}",
                    node.name
                )
            })?;
            Ok(Some(v))
        }
        None => Ok(None),
    }
}

/// PROVISION (rev0.4 authoring step): author a deterministic validator for `criterion` via the writer
/// skill, then vault it **UNAPPROVED**, returning its pin. Approval is a SEPARATE, explicit step
/// ([`approve_and_store`]) so a human/council review sits between authoring and any execution — the
/// authored script never becomes gate-ready in one call. (Runs the writer skill: a live-CLI call.)
pub fn provision_validator(
    criterion: &str,
    runner: &dyn StepRunner,
    store: &mut SqliteStore,
) -> anyhow::Result<String> {
    let v = author_deterministic_validator(criterion, runner)?; // approved == false
    store_validator(store, &v)
}

/// APPROVE a vaulted validator (the human/council step): load it by `pin`, mark it approved, and
/// re-vault. Approving changes the `approved` byte, so the approved copy has a DISTINCT pin — returned.
/// `Ok(None)` if `pin` is unknown. The two pins (unapproved vs approved) are the audit trail: only the
/// approved pin can gate, and a phase carries THAT pin.
pub fn approve_and_store(store: &mut SqliteStore, pin: &str) -> anyhow::Result<Option<String>> {
    let Some(v) = load_validator(store, pin)? else {
        return Ok(None);
    };
    let approved = v.approve();
    Ok(Some(store_validator(store, &approved)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> DeterministicValidator {
        DeterministicValidator {
            criterion: "README exists with a Status section".to_string(),
            script: "test -f README.md && grep -q '## Status' README.md".to_string(),
            approved: true,
        }
    }

    #[test]
    fn pin_is_deterministic_and_content_addressed() {
        let v = sample();
        // Same validator ⇒ same pin (stable, content-addressed).
        assert_eq!(pin(&v), pin(&v.clone()));
        // A different SCRIPT ⇒ a different pin (a swapped script can't reuse an approved pin).
        let mut other = v.clone();
        other.script = "test -f OTHER.md".to_string();
        assert_ne!(
            pin(&v),
            pin(&other),
            "changing the script must change the pin"
        );
        // The approval flag is part of the identity too.
        let mut unappr = v.clone();
        unappr.approved = false;
        assert_ne!(
            pin(&v),
            pin(&unappr),
            "the approved flag is part of the pin"
        );
        // The pin is a 16-char lowercase hex sha256 prefix.
        let p = pin(&v);
        assert_eq!(p.len(), 16);
        assert!(p.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn store_then_load_round_trips_preserving_approved() {
        use wicked_apps_core::open_store;
        let dir = std::env::temp_dir().join("wicked-core-vault-test");
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("vault.db");
        let _ = std::fs::remove_file(&db);
        let mut store = open_store(Some(db.to_str().unwrap())).expect("open_store");

        let v = sample();
        let p = store_validator(&mut store, &v).expect("store");
        assert_eq!(p, pin(&v), "store returns the pin");

        let back = load_validator(&store, &p).expect("load").expect("present");
        assert_eq!(back, v, "the validator round-trips losslessly");
        assert!(back.approved, "the approved flag is preserved on load");

        // An UNAPPROVED validator vaults + loads back unapproved (stored faithfully, not re-approved).
        let unappr = DeterministicValidator {
            criterion: "c".to_string(),
            script: "true".to_string(),
            approved: false,
        };
        let pu = store_validator(&mut store, &unappr).expect("store unapproved");
        let back_u = load_validator(&store, &pu).expect("load").expect("present");
        assert!(
            !back_u.approved,
            "an unapproved validator stays unapproved through the vault"
        );

        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn load_of_an_unknown_pin_is_ok_none() {
        use wicked_apps_core::open_store;
        let dir = std::env::temp_dir().join("wicked-core-vault-unknown-test");
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("vault-unknown.db");
        let _ = std::fs::remove_file(&db);
        let store = open_store(Some(db.to_str().unwrap())).expect("open_store");

        let missing = load_validator(&store, "deadbeefdeadbeef").expect("load must not error");
        assert!(missing.is_none(), "an unknown pin loads as Ok(None)");

        let _ = std::fs::remove_file(&db);
    }

    /// LIVE end-to-end of the rev0.4 provision → approve → pin → re-verify flow against real `claude`.
    #[test]
    #[ignore = "requires real `claude` + installed wicked-testing skills; run with --ignored"]
    fn provision_approve_and_reverify_end_to_end() {
        use crate::execute_wrapped::WrappedCliStepRunner;
        use crate::validator::run_validator;
        use wicked_apps_core::open_store;

        let runner = WrappedCliStepRunner::default();
        let base = std::env::temp_dir().join(format!("wicked-vault-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let mut store = open_store(Some(base.join("v.db").to_str().unwrap())).unwrap();

        // 1. PROVISION: the writer skill authors a check; it's vaulted UNAPPROVED.
        let pin_u =
            provision_validator("a file named greeting.txt exists", &runner, &mut store).unwrap();
        let unapproved = load_validator(&store, &pin_u).unwrap().unwrap();
        assert!(
            !unapproved.approved,
            "authored validator is vaulted UNAPPROVED"
        );
        // An unapproved validator refuses to run (fail-closed).
        let wt = base.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join("greeting.txt"), "hi").unwrap();
        assert!(
            run_validator(&unapproved, &wt).is_err(),
            "unapproved validator must refuse to run"
        );

        // 2. APPROVE (the human/council step) → a DISTINCT pin.
        let pin_a = approve_and_store(&mut store, &pin_u).unwrap().unwrap();
        assert_ne!(pin_a, pin_u, "approval changes the content hash → new pin");
        let approved = load_validator(&store, &pin_a).unwrap().unwrap();
        assert!(approved.approved);

        // 3. RE-VERIFY the approved, pinned validator against a worktree — passes where satisfied,
        //    fails where not.
        assert!(
            run_validator(&approved, &wt).unwrap(),
            "approved validator passes where greeting.txt exists: {}",
            approved.script
        );
        let empty = base.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(
            !run_validator(&approved, &empty).unwrap(),
            "and fails where it does not"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
