//! LOCK A (production cfg): proves the production `production_arming_criteria()`
//! source is fail-closed in a non-`cfg(test)` build.
//!
//! This is an INTEGRATION test: it links `base_mev_trader` as an external crate, so
//! it compiles the library under `#[cfg(not(test))]` — where `OWNER_ATTEST_ADDRESS`
//! is `None`. The safety.rs unit tests CANNOT observe this branch: under `cfg(test)`
//! the trust root is `Some(TEST_OWNER_ADDRESS)`. Together with the safety.rs Lock B
//! unit test (explicit owner + placeholder signature), the two INDEPENDENT locks
//! prove production is unarmed for BOTH structural reasons (unset owner AND a
//! placeholder signature), with no OR assertion.
//!
//! Living in `tests/` (not `src/`), this file does not affect the crate's
//! `capability_seal.rs` `SOURCE_FILES` seal.

use base_mev_trader::{UnarmedReason, production_arming_criteria};

#[test]
fn production_arming_criteria_is_unarmed_with_owner_unset() {
    let criteria = production_arming_criteria();
    // Structural lock A: the compile-time trust root is unset in a production build.
    assert!(!criteria.is_armed(), "production arming criteria must never be armed pre-G4");
    assert_eq!(
        criteria.unarmed_reason(),
        Some(UnarmedReason::OwnerAddressUnset),
        "production (non-test) build must close on the unset owner address before signature parsing"
    );
}
