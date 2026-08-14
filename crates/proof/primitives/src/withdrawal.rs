//! Types used to authorize attested withdrawals.

use alloy_primitives::{B256, b256};

/// `keccak256("BASE_ATTESTED_WITHDRAWAL_V1")`.
pub const ATTESTED_WITHDRAWAL_DOMAIN: B256 =
    b256!("dfde94b4647eb9a297f9ae987fc627da089da546f31d613568fbe0238be65042");

/// Storage slot for `attestedWithdrawals` in `L2ToL1MessagePasser`.
pub const ATTESTED_WITHDRAWAL_SLOT: u64 = 2;

/// A withdrawal authorization signed by an enclave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WithdrawalAuthorization {
    /// Hash of the L2 withdrawal authorization fields.
    pub auth_hash: B256,
}

impl WithdrawalAuthorization {
    /// Return `DOMAIN_TAG || authHash`, the raw data passed to the signer.
    #[must_use]
    pub fn signing_preimage(&self) -> [u8; 64] {
        let mut preimage = [0_u8; 64];
        preimage[..32].copy_from_slice(ATTESTED_WITHDRAWAL_DOMAIN.as_slice());
        preimage[32..].copy_from_slice(self.auth_hash.as_slice());
        preimage
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{b256, keccak256};

    use super::*;
    use crate::PROOF_JOURNAL_BASE_LENGTH;

    #[test]
    fn domain_matches_tag() {
        assert_eq!(ATTESTED_WITHDRAWAL_DOMAIN, keccak256("BASE_ATTESTED_WITHDRAWAL_V1"));
    }

    #[test]
    fn signing_preimage_is_domain_then_auth_hash() {
        let authorization = WithdrawalAuthorization {
            auth_hash: b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        };

        let preimage = authorization.signing_preimage();
        assert_eq!(preimage.len(), 64);
        assert_eq!(&preimage[..32], ATTESTED_WITHDRAWAL_DOMAIN.as_slice());
        assert_eq!(&preimage[32..], authorization.auth_hash.as_slice());
        assert_ne!(preimage.len(), PROOF_JOURNAL_BASE_LENGTH);
    }
}
