//! LOCK A (production cfg): proves the production `production_arming_criteria()`
//! source evaluates ARMED post-G4 in a non-`cfg(test)` build.
//!
//! This is an INTEGRATION test: it links `base_mev_trader` as an external crate, so
//! it compiles the library under `#[cfg(not(test))]` — where `OWNER_ATTEST_ADDRESS`
//! is the pinned G4 owner-attest address and `OWNER_ARM_SIGNATURE` is the real owner
//! signature. The safety.rs unit tests CANNOT observe this branch: under `cfg(test)`
//! the trust root is `Some(TEST_OWNER_ADDRESS)`. This lock proves the production
//! canonical payload/commit/signature triple recovers to the pinned owner and arms.
//!
//! Living in `tests/` (not `src/`), this file does not affect the crate's
//! `capability_seal.rs` `SOURCE_FILES` seal.

use base_mev_trader::production_arming_criteria;

#[test]
fn production_arming_criteria_is_armed_with_g4_owner_and_signature() {
    let criteria = production_arming_criteria();
    // Structural lock A: post-G4 the compile-time trust root is the pinned owner
    // address and the arm signature recovers to it, so the criteria evaluate armed.
    assert!(
        criteria.is_armed(),
        "production arming criteria must be armed post-G4 owner-attest pin"
    );
    assert_eq!(
        criteria.unarmed_reason(),
        None,
        "production (non-test) build must report no unarmed reason once the G4 owner + signature are pinned"
    );
}
