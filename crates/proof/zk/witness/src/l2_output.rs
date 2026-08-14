//! OP Stack L2 output root encoding.

use alloy_sol_types::sol;

sol! {
    /// Canonical L2 output root preimage.
    struct L2Output {
        uint64 zero;
        bytes32 l2_state_root;
        bytes32 l2_storage_hash;
        bytes32 l2_claim_hash;
    }
}
