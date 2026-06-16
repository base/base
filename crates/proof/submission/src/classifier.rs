//! Known revert classification for proof submission transactions.

use base_proof_contracts::{
    already_proven_selector, game_already_exists_selector, invalid_parent_game_selector,
    invalid_signer_selector, l1_origin_too_old_selector,
};
use base_tx_manager::TxManagerError;

use crate::ProofSubmissionError;

const GAME_ALREADY_EXISTS: &str = "GameAlreadyExists";
const ALREADY_PROVEN: &str = "AlreadyProven";
const L1_ORIGIN_TOO_OLD: &str = "L1OriginTooOld";
const INVALID_PARENT_GAME: &str = "InvalidParentGame";
const INVALID_SIGNER: &str = "InvalidSigner";

impl ProofSubmissionError {
    /// Classifies a transaction manager error into a proof submission error.
    ///
    /// The classifier checks structured execution reverts first, matching both
    /// decoded custom-error names and raw selector data. It also preserves the
    /// previous fallback behavior of scanning non-revert transaction manager error
    /// display strings for known custom-error names and selectors.
    pub fn from_tx_manager_error(err: TxManagerError) -> Self {
        let game_exists_selector = game_already_exists_selector();
        let already_proven = already_proven_selector();
        let l1_origin_selector = l1_origin_too_old_selector();
        let invalid_parent_selector = invalid_parent_game_selector();
        let invalid_signer = invalid_signer_selector();

        if let TxManagerError::ExecutionReverted { ref reason, ref data } = err {
            if reason.as_deref().is_some_and(|r| r.contains(GAME_ALREADY_EXISTS)) {
                return Self::GameAlreadyExists;
            }
            if data.as_ref().is_some_and(|d| d.starts_with(&game_exists_selector)) {
                return Self::GameAlreadyExists;
            }
            if reason.as_deref().is_some_and(|r| r.contains(ALREADY_PROVEN)) {
                return Self::ProofAlreadyVerified;
            }
            if data.as_ref().is_some_and(|d| d.starts_with(&already_proven)) {
                return Self::ProofAlreadyVerified;
            }
            if reason.as_deref().is_some_and(|r| r.contains(L1_ORIGIN_TOO_OLD)) {
                return Self::L1OriginTooOld;
            }
            if data.as_ref().is_some_and(|d| d.starts_with(&l1_origin_selector)) {
                return Self::L1OriginTooOld;
            }
            if reason.as_deref().is_some_and(|r| r.contains(INVALID_PARENT_GAME)) {
                return Self::InvalidParentGame;
            }
            if data.as_ref().is_some_and(|d| d.starts_with(&invalid_parent_selector)) {
                return Self::InvalidParentGame;
            }
            if reason.as_deref().is_some_and(|r| r.contains(INVALID_SIGNER)) {
                return Self::InvalidSigner;
            }
            if data.as_ref().is_some_and(|d| d.starts_with(&invalid_signer)) {
                return Self::InvalidSigner;
            }
            return Self::TxManager(err);
        }

        let msg = err.to_string();
        if msg.contains(&alloy_primitives::hex::encode(game_exists_selector))
            || msg.contains(GAME_ALREADY_EXISTS)
        {
            return Self::GameAlreadyExists;
        }
        if msg.contains(&alloy_primitives::hex::encode(already_proven))
            || msg.contains(ALREADY_PROVEN)
        {
            return Self::ProofAlreadyVerified;
        }
        if msg.contains(&alloy_primitives::hex::encode(l1_origin_selector))
            || msg.contains(L1_ORIGIN_TOO_OLD)
        {
            return Self::L1OriginTooOld;
        }
        if msg.contains(&alloy_primitives::hex::encode(invalid_parent_selector))
            || msg.contains(INVALID_PARENT_GAME)
        {
            return Self::InvalidParentGame;
        }
        if msg.contains(&alloy_primitives::hex::encode(invalid_signer))
            || msg.contains(INVALID_SIGNER)
        {
            return Self::InvalidSigner;
        }

        Self::TxManager(err)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, Bytes};
    use base_tx_manager::TxManagerError;
    use rstest::rstest;

    use super::*;

    #[derive(Debug)]
    enum ExpectedClassification {
        GameAlreadyExists,
        ProofAlreadyVerified,
        L1OriginTooOld,
        InvalidParentGame,
        InvalidSigner,
        TxManager,
    }

    #[rstest]
    #[case::rpc_with_game_already_exists_selector_hex(
        TxManagerError::Rpc(format!("execution reverted: 0x{}", alloy_primitives::hex::encode(base_proof_contracts::game_already_exists_selector()))),
        ExpectedClassification::GameAlreadyExists,
        "GameAlreadyExists selector hex in Rpc message"
    )]
    #[case::rpc_with_game_already_exists_name(
        TxManagerError::Rpc(format!("{GAME_ALREADY_EXISTS}()")),
        ExpectedClassification::GameAlreadyExists,
        "GameAlreadyExists name in Rpc message"
    )]
    #[case::reverted_with_game_already_exists_reason(
        TxManagerError::ExecutionReverted {
            reason: Some(format!("{GAME_ALREADY_EXISTS}()")),
            data: None,
        },
        ExpectedClassification::GameAlreadyExists,
        "GameAlreadyExists reason string contains name"
    )]
    #[case::reverted_with_game_already_exists_selector_data(
        {
            let mut data = base_proof_contracts::game_already_exists_selector().to_vec();
            data.extend_from_slice(&[0u8; 32]);
            TxManagerError::ExecutionReverted {
                reason: None,
                data: Some(Bytes::from(data)),
            }
        },
        ExpectedClassification::GameAlreadyExists,
        "GameAlreadyExists raw data contains selector"
    )]
    #[case::rpc_with_already_proven_selector_hex(
        TxManagerError::Rpc(format!("execution reverted: 0x{}", alloy_primitives::hex::encode(base_proof_contracts::already_proven_selector()))),
        ExpectedClassification::ProofAlreadyVerified,
        "AlreadyProven selector hex in Rpc message"
    )]
    #[case::rpc_with_already_proven_name(
        TxManagerError::Rpc(format!("{ALREADY_PROVEN}()")),
        ExpectedClassification::ProofAlreadyVerified,
        "AlreadyProven name in Rpc message"
    )]
    #[case::reverted_with_already_proven_reason(
        TxManagerError::ExecutionReverted {
            reason: Some(format!("{ALREADY_PROVEN}()")),
            data: None,
        },
        ExpectedClassification::ProofAlreadyVerified,
        "AlreadyProven reason string contains name"
    )]
    #[case::reverted_with_already_proven_selector_data(
        TxManagerError::ExecutionReverted {
            reason: None,
            data: Some(Bytes::from(base_proof_contracts::already_proven_selector().to_vec())),
        },
        ExpectedClassification::ProofAlreadyVerified,
        "AlreadyProven raw data contains selector"
    )]
    #[case::rpc_with_l1_origin_selector_hex(
        TxManagerError::Rpc(format!("execution reverted: 0x{}", alloy_primitives::hex::encode(base_proof_contracts::l1_origin_too_old_selector()))),
        ExpectedClassification::L1OriginTooOld,
        "L1OriginTooOld selector hex in Rpc message"
    )]
    #[case::rpc_with_l1_origin_name(
        TxManagerError::Rpc(format!("{L1_ORIGIN_TOO_OLD}()")),
        ExpectedClassification::L1OriginTooOld,
        "L1OriginTooOld name in Rpc message"
    )]
    #[case::reverted_with_l1_origin_reason(
        TxManagerError::ExecutionReverted {
            reason: Some(format!("{L1_ORIGIN_TOO_OLD}()")),
            data: None,
        },
        ExpectedClassification::L1OriginTooOld,
        "L1OriginTooOld reason string contains name"
    )]
    #[case::reverted_with_l1_origin_selector_data(
        TxManagerError::ExecutionReverted {
            reason: None,
            data: Some(Bytes::from(base_proof_contracts::l1_origin_too_old_selector().to_vec())),
        },
        ExpectedClassification::L1OriginTooOld,
        "L1OriginTooOld raw data contains selector"
    )]
    #[case::rpc_with_invalid_parent_selector_hex(
        TxManagerError::Rpc(format!("execution reverted: 0x{}", alloy_primitives::hex::encode(base_proof_contracts::invalid_parent_game_selector()))),
        ExpectedClassification::InvalidParentGame,
        "InvalidParentGame selector hex in Rpc message"
    )]
    #[case::rpc_with_invalid_parent_name(
        TxManagerError::Rpc(format!("{INVALID_PARENT_GAME}()")),
        ExpectedClassification::InvalidParentGame,
        "InvalidParentGame name in Rpc message"
    )]
    #[case::reverted_with_invalid_parent_reason(
        TxManagerError::ExecutionReverted {
            reason: Some(format!("{INVALID_PARENT_GAME}()")),
            data: None,
        },
        ExpectedClassification::InvalidParentGame,
        "InvalidParentGame reason string contains name"
    )]
    #[case::reverted_with_invalid_parent_selector_data(
        TxManagerError::ExecutionReverted {
            reason: None,
            data: Some(Bytes::from(base_proof_contracts::invalid_parent_game_selector().to_vec())),
        },
        ExpectedClassification::InvalidParentGame,
        "InvalidParentGame raw data contains selector"
    )]
    #[case::rpc_with_invalid_signer_selector_hex(
        TxManagerError::Rpc(format!("execution reverted: 0x{}", alloy_primitives::hex::encode(base_proof_contracts::invalid_signer_selector()))),
        ExpectedClassification::InvalidSigner,
        "InvalidSigner selector hex in Rpc message"
    )]
    #[case::rpc_with_invalid_signer_name(
        TxManagerError::Rpc(format!("{INVALID_SIGNER}(0x0000000000000000000000000000000000000000)")),
        ExpectedClassification::InvalidSigner,
        "InvalidSigner name in Rpc message"
    )]
    #[case::reverted_with_invalid_signer_reason(
        TxManagerError::ExecutionReverted {
            reason: Some(format!("{INVALID_SIGNER}(0x0000000000000000000000000000000000000000)")),
            data: None,
        },
        ExpectedClassification::InvalidSigner,
        "InvalidSigner reason string contains name"
    )]
    #[case::reverted_with_invalid_signer_selector_data(
        {
            let mut data = base_proof_contracts::invalid_signer_selector().to_vec();
            data.extend_from_slice(Address::ZERO.as_slice());
            TxManagerError::ExecutionReverted {
                reason: None,
                data: Some(Bytes::from(data)),
            }
        },
        ExpectedClassification::InvalidSigner,
        "InvalidSigner raw data contains selector"
    )]
    #[case::reverted_other_error(
        TxManagerError::ExecutionReverted {
            reason: Some("SomeOtherError()".to_string()),
            data: Some(Bytes::from(vec![0xde, 0xad, 0xbe, 0xef])),
        },
        ExpectedClassification::TxManager,
        "unrelated revert"
    )]
    #[case::nonce_too_low(
        TxManagerError::NonceTooLow,
        ExpectedClassification::TxManager,
        "non-revert error"
    )]
    fn classify_tx_manager_error_maps_known_reverts(
        #[case] err: TxManagerError,
        #[case] expected: ExpectedClassification,
        #[case] scenario: &str,
    ) {
        let result = ProofSubmissionError::from_tx_manager_error(err);

        match expected {
            ExpectedClassification::GameAlreadyExists => assert!(
                matches!(result, ProofSubmissionError::GameAlreadyExists),
                "{scenario}: expected GameAlreadyExists, got {result:?}"
            ),
            ExpectedClassification::ProofAlreadyVerified => assert!(
                matches!(result, ProofSubmissionError::ProofAlreadyVerified),
                "{scenario}: expected ProofAlreadyVerified, got {result:?}"
            ),
            ExpectedClassification::L1OriginTooOld => assert!(
                matches!(result, ProofSubmissionError::L1OriginTooOld),
                "{scenario}: expected L1OriginTooOld, got {result:?}"
            ),
            ExpectedClassification::InvalidParentGame => assert!(
                matches!(result, ProofSubmissionError::InvalidParentGame),
                "{scenario}: expected InvalidParentGame, got {result:?}"
            ),
            ExpectedClassification::InvalidSigner => assert!(
                matches!(result, ProofSubmissionError::InvalidSigner),
                "{scenario}: expected InvalidSigner, got {result:?}"
            ),
            ExpectedClassification::TxManager => assert!(
                matches!(result, ProofSubmissionError::TxManager(_)),
                "{scenario}: expected TxManager, got {result:?}"
            ),
        }
    }
}
