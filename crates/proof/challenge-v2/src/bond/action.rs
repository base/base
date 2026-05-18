//! Bond action types and L1 calldata encoding.
//!
//! [`BondAction`] enumerates the on-chain calls the challenger makes
//! while claiming a bond. [`BondRequest`] bundles an action with the
//! game it targets. Produced by `run_bond_worker` and consumed by
//! [`crate::SubmissionTask`].

use std::fmt;

use alloy_primitives::{Address, Bytes};
use base_proof_contracts::{
    encode_claim_credit_calldata, encode_close_game_calldata, encode_resolve_calldata,
};

/// One on-chain call in the bond claim sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BondAction {
    /// Calls `resolve()`.
    Resolve,
    /// First call to `claimCredit()`.
    UnlockCredit,
    /// Second call to `claimCredit()`.
    WithdrawCredit,
    /// Calls `closeGame()`.
    CloseGame,
}

impl BondAction {
    /// Encodes the L1 calldata for this action.
    pub fn to_calldata(self) -> Bytes {
        match self {
            Self::Resolve => encode_resolve_calldata(),
            Self::UnlockCredit | Self::WithdrawCredit => encode_claim_credit_calldata(),
            Self::CloseGame => encode_close_game_calldata(),
        }
    }
}

impl fmt::Display for BondAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Resolve => "Resolve",
            Self::UnlockCredit => "UnlockCredit",
            Self::WithdrawCredit => "WithdrawCredit",
            Self::CloseGame => "CloseGame",
        })
    }
}

/// A [`BondAction`] paired with the game proxy it targets.
///
/// Produced by `run_bond_worker` and consumed by [`crate::SubmissionTask`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BondRequest {
    /// Dispute game proxy this action targets.
    pub game_address: Address,
    /// Action to call on the game contract.
    pub action: BondAction,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_dispatches_to_encode_resolve_calldata() {
        assert_eq!(BondAction::Resolve.to_calldata(), encode_resolve_calldata());
    }

    #[test]
    fn unlock_credit_dispatches_to_encode_claim_credit_calldata() {
        assert_eq!(BondAction::UnlockCredit.to_calldata(), encode_claim_credit_calldata());
    }

    #[test]
    fn withdraw_credit_dispatches_to_encode_claim_credit_calldata() {
        assert_eq!(BondAction::WithdrawCredit.to_calldata(), encode_claim_credit_calldata());
    }

    #[test]
    fn close_game_dispatches_to_encode_close_game_calldata() {
        assert_eq!(BondAction::CloseGame.to_calldata(), encode_close_game_calldata());
    }

    #[test]
    fn unlock_and_withdraw_credit_have_identical_calldata() {
        // Both phases of the two-step claimCredit sequence map to the
        // same on-chain entrypoint; the variants exist for log/metric
        // labelling only.
        assert_eq!(
            BondAction::UnlockCredit.to_calldata(),
            BondAction::WithdrawCredit.to_calldata()
        );
    }

    #[test]
    fn display_labels_each_variant() {
        assert_eq!(BondAction::Resolve.to_string(), "Resolve");
        assert_eq!(BondAction::UnlockCredit.to_string(), "UnlockCredit");
        assert_eq!(BondAction::WithdrawCredit.to_string(), "WithdrawCredit");
        assert_eq!(BondAction::CloseGame.to_string(), "CloseGame");
    }
}
