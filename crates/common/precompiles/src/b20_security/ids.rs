//! Security B-20 role and policy identifiers.

use alloy_primitives::{B256, b256};

pub(super) const SECURITY_OPERATOR_ROLE: B256 =
    b256!("e63901dfe7775ace99fa3654743976eb0ab2009f5d19c4fc1ecd40aed27d59af");
pub(super) const BURN_FROM_ROLE: B256 =
    b256!("25400dba76bf0d00acf274c2b61ff56aa4ed19826e21e0186e3fecd6a6671875");
pub(super) const REDEEM_SENDER_POLICY: B256 =
    b256!("0ff53b08b65363a609bb561211128f4044adc0e351f0b92b6aa23f8d85462f59");

#[cfg(test)]
mod tests {
    use alloy_primitives::keccak256;

    use super::{BURN_FROM_ROLE, REDEEM_SENDER_POLICY, SECURITY_OPERATOR_ROLE};

    #[test]
    fn role_and_policy_ids_match_solidity_hashes() {
        assert_eq!(SECURITY_OPERATOR_ROLE, keccak256("SECURITY_OPERATOR_ROLE"));
        assert_eq!(BURN_FROM_ROLE, keccak256("BURN_FROM_ROLE"));
        assert_eq!(REDEEM_SENDER_POLICY, keccak256("REDEEM_SENDER_POLICY"));
    }
}
