//! Types and encoding helpers for the attested-withdrawal relay.

use std::time::Duration;

use alloy_primitives::{Address, B256, U256, keccak256};
use base_proof_primitives::ATTESTED_WITHDRAWAL_SLOT;
use url::Url;

/// Configuration for the optional attested-withdrawal relay.
#[derive(Debug, Clone)]
pub struct AttestedWithdrawalRelayConfig {
    /// L1 `OptimismPortal2` address.
    pub portal_address: Address,
    /// Private enclave JSON-RPC endpoint.
    pub enclave_rpc_url: Url,
    /// First L2 block to scan.
    pub start_block: u64,
    /// Delay between scans.
    pub poll_interval: Duration,
    /// L2 confirmations required before processing a log.
    pub confirmations: u64,
    /// Maximum number of L2 blocks in one log query.
    pub scan_batch_size: u64,
}

/// Computes the authorization hash emitted by `L2ToL1MessagePasser`.
#[must_use]
pub fn attested_withdrawal_auth_hash(
    l2_chain_id: u64,
    recipient: Address,
    amount: U256,
    nonce: U256,
) -> B256 {
    let mut encoded = [0_u8; 160];
    encoded[24..32].copy_from_slice(&l2_chain_id.to_be_bytes());
    encoded[44..64].copy_from_slice(recipient.as_slice());
    encoded[96..128].copy_from_slice(&amount.to_be_bytes::<32>());
    encoded[128..160].copy_from_slice(&nonce.to_be_bytes::<32>());
    keccak256(encoded)
}

/// Computes the `attestedWithdrawals` mapping key for an authorization hash.
#[must_use]
pub fn attested_withdrawal_storage_slot(auth_hash: B256) -> B256 {
    let mut encoded = [0_u8; 64];
    encoded[..32].copy_from_slice(auth_hash.as_slice());
    encoded[56..].copy_from_slice(&ATTESTED_WITHDRAWAL_SLOT.to_be_bytes());
    keccak256(encoded)
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, b256};

    use super::*;

    #[test]
    fn authorization_hash_matches_solidity_abi_encoding() {
        assert_eq!(
            attested_withdrawal_auth_hash(
                8453,
                address!("1234567890123456789012345678901234567890"),
                U256::from(42),
                U256::from(7),
            ),
            b256!("7a4b6c9bc6a64e02ac9859dfb358d681059014a1761332c57dcfe79917b86fb0")
        );
    }

    #[test]
    fn storage_slot_encodes_auth_hash_then_mapping_slot() {
        assert_eq!(
            attested_withdrawal_storage_slot(b256!(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            )),
            b256!("4c2e3d1f43d041c459d5ee7f8b7a15e09cc4c9ad9f1c49cf8a9de27ec92e496f")
        );
    }
}

/// Decoded attested-withdrawal event fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttestedWithdrawalEvent {
    /// Authorization hash emitted by the L2 message passer.
    pub auth_hash: B256,
    /// L1 recipient.
    pub recipient: Address,
    /// Withdrawal amount in wei.
    pub amount: U256,
    /// Per-message-passer authorization nonce.
    pub nonce: U256,
}

/// Decodes and validates one `AttestedWithdrawalInitiated` log.
pub fn decode_attested_withdrawal_log(
    log: &alloy_rpc_types_eth::Log,
    l2_chain_id: u64,
) -> Result<AttestedWithdrawalEvent, AttestedWithdrawalRelayError> {
    use alloy_sol_types::SolEvent;
    use base_proof_contracts::IL2ToL1MessagePasser;

    let event = IL2ToL1MessagePasser::AttestedWithdrawalInitiated::decode_log_data(&log.inner.data)
        .map_err(|error| AttestedWithdrawalRelayError::InvalidEvent(error.to_string()))?;
    if event.token != Address::ZERO {
        return Err(AttestedWithdrawalRelayError::UnsupportedToken(event.token));
    }
    let expected =
        attested_withdrawal_auth_hash(l2_chain_id, event.recipient, event.amount, event.nonce);
    if event.authHash != expected {
        return Err(AttestedWithdrawalRelayError::AuthorizationHashMismatch {
            expected,
            actual: event.authHash,
        });
    }
    Ok(AttestedWithdrawalEvent {
        auth_hash: event.authHash,
        recipient: event.recipient,
        amount: event.amount,
        nonce: event.nonce,
    })
}

/// Errors raised while validating a withdrawal relay record.
#[derive(Debug, thiserror::Error)]
pub enum AttestedWithdrawalRelayError {
    /// The event payload was not a valid attested-withdrawal event.
    #[error("invalid attested withdrawal event: {0}")]
    InvalidEvent(String),
    /// The event requested a non-ETH transfer.
    #[error("unsupported attested withdrawal token: {0}")]
    UnsupportedToken(Address),
    /// The event hash does not match its authorization fields.
    #[error("attested withdrawal authorization hash mismatch: expected {expected}, got {actual}")]
    AuthorizationHashMismatch {
        /// Expected hash.
        expected: B256,
        /// Hash emitted by L2.
        actual: B256,
    },
}
