//! Pure derivation of the non-broadcastable edge measurement transaction.

use alloy_consensus::private::alloy_rlp::Decodable;
use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_eips::eip2930::AccessList;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, address, keccak256};
use thiserror::Error;

/// Base mainnet chain identifier pinned by the measurement contract.
pub const MEASUREMENT_CHAIN_ID: u64 = 8_453;
/// Fixed gas limit for the measurement-only executor call.
pub const MEASUREMENT_GAS_LIMIT: u64 = 3_000_000;
/// Pinned Blink atomic executor used by the offline measurement envelope.
pub const MEASUREMENT_EXECUTOR: Address = address!("1810cbFA042e8199121021F056Afe8B31028CF55");

/// Snapshot-local committed and pending nonce evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeasurementNonceWitnessV1 {
    /// Nonce read from the committed parent.
    pub committed: u64,
    /// Original nonce of the pending overlay, when an overlay exists.
    pub pending_original: Option<u64>,
    /// Current nonce of the same pending overlay.
    pub pending_current: Option<u64>,
}

impl MeasurementNonceWitnessV1 {
    /// Constructs a committed-only witness.
    pub const fn committed(committed: u64) -> Self {
        Self { committed, pending_original: None, pending_current: None }
    }

    /// Constructs a witness containing a pending overlay.
    pub const fn pending(committed: u64, pending_original: u64, pending_current: u64) -> Self {
        Self {
            committed,
            pending_original: Some(pending_original),
            pending_current: Some(pending_current),
        }
    }

    /// Resolves the exact nonce without consulting a node or network service.
    pub const fn resolve(self) -> Result<u64, MeasurementTxError> {
        match (self.pending_original, self.pending_current) {
            (None, None) => Ok(self.committed),
            (Some(original), Some(current))
                if original == self.committed && current >= original =>
            {
                Ok(current)
            }
            _ => Err(MeasurementTxError::NonceWitnessIncoherent),
        }
    }
}

/// Complete preloaded inputs for one pure unsigned derivation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeasurementTxInputV1 {
    /// Snapshot-local nonce evidence.
    pub nonce: MeasurementNonceWitnessV1,
    /// Base fee from the captured pending snapshot header.
    pub snapshot_base_fee_per_gas: u128,
    /// Victim EIP-1559 maximum fee.
    pub victim_max_fee_per_gas: u128,
    /// Victim EIP-1559 priority fee, reused exactly.
    pub victim_max_priority_fee_per_gas: u128,
    /// Candidate block; the zero-block validity window resolves to this same value.
    pub candidate_block: u64,
    /// Exact preloaded executor calldata.
    pub calldata: Bytes,
    /// Victim hash bound by the candidate context.
    pub victim_hash: B256,
    /// Target hash encoded by the preloaded calldata builder.
    pub calldata_target_hash: B256,
}

/// Fully resolved unsigned measurement envelope and its deterministic bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackrunMeasurementTxV1 {
    /// Unsigned EIP-1559 transaction.
    pub transaction: TxEip1559,
    /// EIP-2718 type byte plus unsigned RLP fields used for signing-hash derivation.
    pub unsigned_envelope_bytes: Bytes,
    /// Keccak-256 of [`Self::unsigned_envelope_bytes`].
    pub unsigned_envelope_hash: B256,
    /// Snapshot base fee used by the checked fee rule.
    pub snapshot_base_fee_per_gas: u128,
    /// Candidate block plus the pinned zero-block validity window.
    pub valid_until_block: u64,
    /// Victim transaction hash bound into the executor calldata.
    pub target_tx_hash: B256,
}

/// Fail-closed errors from pure measurement transaction derivation.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum MeasurementTxError {
    /// Committed and pending nonce evidence was incomplete or incoherent.
    #[error("nonce witness is incoherent")]
    NonceWitnessIncoherent,
    /// The victim priority fee was zero.
    #[error("victim priority fee is zero")]
    ZeroPriorityFee,
    /// Adding the snapshot base fee and victim priority overflowed.
    #[error("measurement max fee overflowed")]
    MaxFeeOverflow,
    /// The derived max fee exceeded the victim max fee.
    #[error("measurement max fee exceeds victim max fee")]
    MaxFeeExceedsVictim,
    /// Preloaded calldata was empty.
    #[error("measurement calldata is empty")]
    EmptyCalldata,
    /// The calldata target did not equal the candidate victim hash.
    #[error("measurement calldata target hash does not match victim")]
    TargetHashMismatch,
    /// Re-encoding the resolved envelope did not reproduce its bytes and hash.
    #[error("measurement envelope reparse mismatch")]
    MeasurementEnvelopeReparseMismatch,
}

/// Pure, same-frame/preloaded-only unsigned transaction derivation.
#[derive(Debug, Default, Clone, Copy)]
pub struct MeasurementTxDeriverV1;

impl MeasurementTxDeriverV1 {
    /// Derives the exact Base measurement transaction without signing, submission, or I/O.
    pub fn derive(
        input: MeasurementTxInputV1,
    ) -> Result<BackrunMeasurementTxV1, MeasurementTxError> {
        let nonce = input.nonce.resolve()?;
        if input.victim_max_priority_fee_per_gas == 0 {
            return Err(MeasurementTxError::ZeroPriorityFee);
        }
        let max_fee_per_gas = input
            .snapshot_base_fee_per_gas
            .checked_add(input.victim_max_priority_fee_per_gas)
            .ok_or(MeasurementTxError::MaxFeeOverflow)?;
        if max_fee_per_gas > input.victim_max_fee_per_gas {
            return Err(MeasurementTxError::MaxFeeExceedsVictim);
        }
        if input.calldata.is_empty() {
            return Err(MeasurementTxError::EmptyCalldata);
        }
        if input.calldata_target_hash != input.victim_hash {
            return Err(MeasurementTxError::TargetHashMismatch);
        }

        let transaction = TxEip1559 {
            chain_id: MEASUREMENT_CHAIN_ID,
            nonce,
            gas_limit: MEASUREMENT_GAS_LIMIT,
            max_fee_per_gas,
            max_priority_fee_per_gas: input.victim_max_priority_fee_per_gas,
            to: TxKind::Call(MEASUREMENT_EXECUTOR),
            value: U256::ZERO,
            access_list: AccessList::default(),
            input: input.calldata,
        };
        let encoded = transaction.encoded_for_signing();
        let envelope_hash = keccak256(&encoded);
        let mut rlp_fields = encoded.get(1..).unwrap_or_default();
        let verification = TxEip1559::decode(&mut rlp_fields)
            .map_err(|_| MeasurementTxError::MeasurementEnvelopeReparseMismatch)?;
        if !rlp_fields.is_empty()
            || verification != transaction
            || verification.encoded_for_signing() != encoded
            || verification.signature_hash() != envelope_hash
        {
            return Err(MeasurementTxError::MeasurementEnvelopeReparseMismatch);
        }

        Ok(BackrunMeasurementTxV1 {
            transaction,
            unsigned_envelope_bytes: encoded.into(),
            unsigned_envelope_hash: envelope_hash,
            snapshot_base_fee_per_gas: input.snapshot_base_fee_per_gas,
            valid_until_block: input.candidate_block,
            target_tx_hash: input.victim_hash,
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::Transaction;
    use alloy_primitives::{b256, bytes};

    use super::*;

    fn input() -> MeasurementTxInputV1 {
        MeasurementTxInputV1 {
            nonce: MeasurementNonceWitnessV1::pending(4, 4, 7),
            snapshot_base_fee_per_gas: 10,
            victim_max_fee_per_gas: 12,
            victim_max_priority_fee_per_gas: 2,
            candidate_block: 100,
            calldata: bytes!("1234"),
            victim_hash: b256!("1111111111111111111111111111111111111111111111111111111111111111"),
            calldata_target_hash: b256!(
                "1111111111111111111111111111111111111111111111111111111111111111"
            ),
        }
    }

    #[test]
    fn derives_exact_pinned_unsigned_envelope() {
        let derived = MeasurementTxDeriverV1::derive(input()).expect("measurement transaction");
        assert_eq!(derived.transaction.chain_id(), Some(MEASUREMENT_CHAIN_ID));
        assert_eq!(derived.transaction.nonce(), 7);
        assert_eq!(derived.transaction.gas_limit(), MEASUREMENT_GAS_LIMIT);
        assert_eq!(derived.transaction.max_fee_per_gas(), 12);
        assert_eq!(derived.transaction.max_priority_fee_per_gas(), Some(2));
        assert_eq!(derived.transaction.to(), Some(MEASUREMENT_EXECUTOR));
        assert_eq!(derived.valid_until_block, 100);
        assert_eq!(
            derived.unsigned_envelope_bytes,
            bytes!("02e482210507020c832dc6c0941810cbfa042e8199121021f056afe8b31028cf5580821234c0")
        );
        assert_eq!(keccak256(&derived.unsigned_envelope_bytes), derived.unsigned_envelope_hash);
    }

    #[test]
    fn rejects_nonce_fee_and_target_substitution() {
        let mut invalid = input();
        invalid.nonce = MeasurementNonceWitnessV1::pending(4, 3, 7);
        assert_eq!(
            MeasurementTxDeriverV1::derive(invalid),
            Err(MeasurementTxError::NonceWitnessIncoherent)
        );

        let mut invalid = input();
        invalid.victim_max_fee_per_gas = 11;
        assert_eq!(
            MeasurementTxDeriverV1::derive(invalid),
            Err(MeasurementTxError::MaxFeeExceedsVictim)
        );

        let mut invalid = input();
        invalid.calldata_target_hash = B256::ZERO;
        assert_eq!(
            MeasurementTxDeriverV1::derive(invalid),
            Err(MeasurementTxError::TargetHashMismatch)
        );
    }
}
