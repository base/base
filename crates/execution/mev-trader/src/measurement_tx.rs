//! Pure derivation of non-envelope edge measurement evidence bindings.

use alloy_consensus::private::alloy_rlp::{Decodable, Header};
use alloy_primitives::{Address, B256, Bytes, U256, address, keccak256};
use revm::precompile::{Crypto, DefaultCrypto};
use thiserror::Error;

use crate::{BackrunPlan, ExactProtocol, MeasurementEncoder};

/// Base mainnet chain identifier pinned by the measurement evidence contract.
pub const MEASUREMENT_CHAIN_ID: u64 = 8_453;
/// Pinned Blink atomic executor whose identity is covered by measurement evidence.
pub const MEASUREMENT_EXECUTOR: Address = address!("1810cbFA042e8199121021F056Afe8B31028CF55");

/// Hash-pinned snapshot-local committed and pending nonce evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotNonceWitnessV1 {
    /// Parent block hash pinning every nonce value.
    pub parent_block_hash: B256,
    /// Parent block number pinning every nonce value.
    pub parent_block_number: u64,
    /// Nonce read from the committed parent.
    pub committed: u64,
    /// Original nonce of the pending overlay, when an overlay exists.
    pub pending_original: Option<u64>,
    /// Current nonce of the same pending overlay.
    pub pending_current: Option<u64>,
}

impl SnapshotNonceWitnessV1 {
    /// Constructs a hash-pinned committed-only witness.
    pub const fn committed(
        parent_block_hash: B256,
        parent_block_number: u64,
        committed: u64,
    ) -> Self {
        Self {
            parent_block_hash,
            parent_block_number,
            committed,
            pending_original: None,
            pending_current: None,
        }
    }

    /// Constructs a hash-pinned witness containing a pending overlay.
    pub const fn pending(
        parent_block_hash: B256,
        parent_block_number: u64,
        committed: u64,
        pending_original: u64,
        pending_current: u64,
    ) -> Self {
        Self {
            parent_block_hash,
            parent_block_number,
            committed,
            pending_original: Some(pending_original),
            pending_current: Some(pending_current),
        }
    }

    /// Resolves the exact nonce without consulting a node or network service.
    pub const fn resolve(self) -> Result<u64, MeasurementBindingError> {
        match (self.pending_original, self.pending_current) {
            (None, None) => Ok(self.committed),
            (Some(original), Some(current))
                if original == self.committed && current >= original =>
            {
                Ok(current)
            }
            _ => Err(MeasurementBindingError::NonceWitnessIncoherent),
        }
    }
}

/// Per-hop executor evidence absent from the selected measurement plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeasurementExecutionHopV1 {
    /// Validated adapter address.
    pub adapter: Address,
    /// Exact output floor for this hop.
    pub min_amount_out: U256,
    /// Validated funding target.
    pub funding_target: Address,
}

/// Complete preloaded inputs for one pure evidence binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeasurementBindingInputV1 {
    /// Snapshot-local nonce evidence.
    pub nonce: SnapshotNonceWitnessV1,
    /// Base fee from the captured pending snapshot header.
    pub snapshot_base_fee_per_gas: u128,
    /// Selected same-frame two-hop plan.
    pub plan: BackrunPlan,
    /// Executor evidence validated by the edge owner.
    pub execution_hops: [MeasurementExecutionHopV1; 2],
    /// Exact received victim bytes used only as immutable source evidence.
    pub victim_raw: Bytes,
}

/// Deterministic non-envelope binding between snapshot, plan, victim fees, and execution evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackrunMeasurementBindingV1 {
    /// Full hash-pinned nonce witness, including original and current overlay values.
    pub nonce_witness: SnapshotNonceWitnessV1,
    /// Selected nonce retained separately for mutation detection.
    pub selected_nonce: u64,
    /// Snapshot base fee used by the checked fee rule.
    pub snapshot_base_fee_per_gas: u128,
    /// Victim priority fee used by the checked fee rule.
    pub victim_max_priority_fee_per_gas: u128,
    /// Victim maximum fee bounding the checked fee rule.
    pub victim_max_fee_per_gas: u128,
    /// Candidate block plus the pinned zero-block validity window.
    pub valid_until_block: u64,
    /// Victim hash bound into the candidate.
    pub target_tx_hash: B256,
    /// Same-frame selected plan retained for query-free revalidation.
    pub plan: BackrunPlan,
    /// Same-frame execution evidence retained for query-free revalidation.
    pub execution_hops: [MeasurementExecutionHopV1; 2],
    /// Domain-separated SHA-256 over the non-envelope evidence fields.
    pub binding_digest: B256,
}

/// Fail-closed errors from pure measurement evidence derivation.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum MeasurementBindingError {
    /// Committed and pending nonce evidence was incomplete or incoherent.
    #[error("nonce witness is incoherent")]
    NonceWitnessIncoherent,
    /// The selected plan or its execution evidence was invalid.
    #[error("selected measurement plan or execution evidence is invalid")]
    InvalidExecutionPlan,
    /// Adding the snapshot base fee and victim priority overflowed.
    #[error("measurement fee bound overflowed")]
    MaxFeeOverflow,
    /// The derived fee bound exceeded the victim max fee.
    #[error("measurement fee bound exceeds victim max fee")]
    MaxFeeExceedsVictim,
    /// The victim bytes were not exact EIP-1559 source evidence bound to the plan.
    #[error("victim fee evidence is malformed or mismatched")]
    VictimEvidenceMismatch,
    /// Retained evidence no longer reproduced its deterministic binding.
    #[error("measurement evidence binding mismatch")]
    BindingMismatch,
}

/// Pure, same-frame/preloaded-only measurement evidence derivation.
#[derive(Debug, Default, Clone, Copy)]
pub struct MeasurementBindingDeriverV1;

impl MeasurementBindingDeriverV1 {
    /// Binds exact local evidence without constructing a transaction, signing payload, or envelope.
    pub fn bind(
        input: MeasurementBindingInputV1,
    ) -> Result<BackrunMeasurementBindingV1, MeasurementBindingError> {
        MeasurementEncoder::validate(&input.plan)
            .map_err(|_| MeasurementBindingError::InvalidExecutionPlan)?;
        Self::validate_execution_hops(&input.plan, &input.execution_hops)?;
        let selected_nonce = input.nonce.resolve()?;
        if input.nonce.parent_block_hash != input.plan.parent_hash
            || input.nonce.parent_block_number.checked_add(1) != Some(input.plan.block_number)
        {
            return Err(MeasurementBindingError::NonceWitnessIncoherent);
        }
        let (victim_chain_id, victim_priority, victim_max_fee) =
            Self::parse_victim_fee_evidence(&input.victim_raw)?;
        let target_tx_hash = keccak256(&input.victim_raw);
        if victim_chain_id != MEASUREMENT_CHAIN_ID || target_tx_hash != input.plan.victim {
            return Err(MeasurementBindingError::VictimEvidenceMismatch);
        }
        let fee_bound = input
            .snapshot_base_fee_per_gas
            .checked_add(victim_priority)
            .ok_or(MeasurementBindingError::MaxFeeOverflow)?;
        if fee_bound > victim_max_fee {
            return Err(MeasurementBindingError::MaxFeeExceedsVictim);
        }
        let valid_until_block = input.plan.block_number;
        let mut binding = BackrunMeasurementBindingV1 {
            nonce_witness: input.nonce,
            selected_nonce,
            snapshot_base_fee_per_gas: input.snapshot_base_fee_per_gas,
            victim_max_priority_fee_per_gas: victim_priority,
            victim_max_fee_per_gas: victim_max_fee,
            valid_until_block,
            target_tx_hash,
            plan: input.plan,
            execution_hops: input.execution_hops,
            binding_digest: B256::ZERO,
        };
        binding.binding_digest = Self::binding_digest(&binding);
        Ok(binding)
    }

    /// Revalidates every retained binding field against the separately retained victim evidence.
    pub fn validate(
        binding: &BackrunMeasurementBindingV1,
        victim_raw: &Bytes,
    ) -> Result<(), MeasurementBindingError> {
        let expected = Self::bind(MeasurementBindingInputV1 {
            nonce: binding.nonce_witness,
            snapshot_base_fee_per_gas: binding.snapshot_base_fee_per_gas,
            plan: binding.plan.clone(),
            execution_hops: binding.execution_hops,
            victim_raw: victim_raw.clone(),
        })?;
        if &expected != binding {
            return Err(MeasurementBindingError::BindingMismatch);
        }
        Ok(())
    }

    /// Extracts only chain and fee literals from exact received EIP-1559 source bytes.
    pub fn parse_victim_fee_evidence(
        raw: &[u8],
    ) -> Result<(u64, u128, u128), MeasurementBindingError> {
        let Some((&transaction_type, encoded_fields)) = raw.split_first() else {
            return Err(MeasurementBindingError::VictimEvidenceMismatch);
        };
        if transaction_type != 2 {
            return Err(MeasurementBindingError::VictimEvidenceMismatch);
        }
        let mut encoded_fields = encoded_fields;
        let header = Header::decode(&mut encoded_fields)
            .map_err(|_| MeasurementBindingError::VictimEvidenceMismatch)?;
        if !header.list || encoded_fields.len() != header.payload_length {
            return Err(MeasurementBindingError::VictimEvidenceMismatch);
        }
        let mut fields = encoded_fields;
        let chain_id = u64::decode(&mut fields)
            .map_err(|_| MeasurementBindingError::VictimEvidenceMismatch)?;
        u64::decode(&mut fields).map_err(|_| MeasurementBindingError::VictimEvidenceMismatch)?;
        let priority = u128::decode(&mut fields)
            .map_err(|_| MeasurementBindingError::VictimEvidenceMismatch)?;
        let max_fee = u128::decode(&mut fields)
            .map_err(|_| MeasurementBindingError::VictimEvidenceMismatch)?;
        u64::decode(&mut fields).map_err(|_| MeasurementBindingError::VictimEvidenceMismatch)?;
        for _ in 0..7 {
            Self::skip_rlp_item(&mut fields)?;
        }
        if !fields.is_empty() {
            return Err(MeasurementBindingError::VictimEvidenceMismatch);
        }
        Ok((chain_id, priority, max_fee))
    }

    /// Validates plan-to-hop consistency without producing executor input bytes.
    pub fn validate_execution_hops(
        plan: &BackrunPlan,
        execution_hops: &[MeasurementExecutionHopV1; 2],
    ) -> Result<(), MeasurementBindingError> {
        for (hop, evidence) in plan.route.iter().zip(execution_hops) {
            if evidence.adapter.is_zero()
                || evidence.funding_target.is_zero()
                || hop.pool.is_zero()
                || hop.token_in.is_zero()
                || hop.token_out.is_zero()
                || hop.token_in == hop.token_out
            {
                return Err(MeasurementBindingError::InvalidExecutionPlan);
            }
            if matches!(hop.protocol, ExactProtocol::UniswapV2 | ExactProtocol::AerodromeVolatile)
                && !hop.fee_pips.is_multiple_of(100)
            {
                return Err(MeasurementBindingError::InvalidExecutionPlan);
            }
        }
        Ok(())
    }

    /// Computes the domain-separated fixed-order evidence digest.
    pub fn binding_digest(binding: &BackrunMeasurementBindingV1) -> B256 {
        let mut bytes = Vec::with_capacity(384);
        bytes.extend_from_slice(b"base-edge-measurement-binding-v1\0");
        bytes.extend_from_slice(binding.nonce_witness.parent_block_hash.as_slice());
        bytes.extend_from_slice(&binding.nonce_witness.parent_block_number.to_be_bytes());
        bytes.extend_from_slice(&binding.nonce_witness.committed.to_be_bytes());
        Self::push_optional_u64(&mut bytes, binding.nonce_witness.pending_original);
        Self::push_optional_u64(&mut bytes, binding.nonce_witness.pending_current);
        bytes.extend_from_slice(&binding.selected_nonce.to_be_bytes());
        bytes.extend_from_slice(&binding.snapshot_base_fee_per_gas.to_be_bytes());
        bytes.extend_from_slice(&binding.victim_max_priority_fee_per_gas.to_be_bytes());
        bytes.extend_from_slice(&binding.victim_max_fee_per_gas.to_be_bytes());
        bytes.extend_from_slice(&binding.valid_until_block.to_be_bytes());
        bytes.extend_from_slice(binding.target_tx_hash.as_slice());
        bytes.extend_from_slice(MEASUREMENT_EXECUTOR.as_slice());
        bytes.extend_from_slice(binding.plan.digest.0.as_slice());
        for (hop, evidence) in binding.plan.route.iter().zip(&binding.execution_hops) {
            bytes.extend_from_slice(hop.pool.as_slice());
            bytes.push(hop.protocol as u8);
            bytes.extend_from_slice(hop.token_in.as_slice());
            bytes.extend_from_slice(hop.token_out.as_slice());
            bytes.extend_from_slice(&hop.fee_pips.to_be_bytes());
            bytes.extend_from_slice(evidence.adapter.as_slice());
            bytes.extend_from_slice(&evidence.min_amount_out.to_be_bytes::<32>());
            bytes.extend_from_slice(evidence.funding_target.as_slice());
        }
        bytes.extend_from_slice(&binding.plan.amount_in.to_be_bytes::<32>());
        bytes.extend_from_slice(&binding.plan.amount_out.to_be_bytes::<32>());
        B256::new(DefaultCrypto.sha256(&bytes))
    }

    /// Advances over one canonical RLP item without materializing it.
    pub fn skip_rlp_item(input: &mut &[u8]) -> Result<(), MeasurementBindingError> {
        let before = input.len();
        let header =
            Header::decode(input).map_err(|_| MeasurementBindingError::VictimEvidenceMismatch)?;
        if input.len() < header.payload_length {
            return Err(MeasurementBindingError::VictimEvidenceMismatch);
        }
        *input = &input[header.payload_length..];
        if input.len() >= before {
            return Err(MeasurementBindingError::VictimEvidenceMismatch);
        }
        Ok(())
    }

    /// Appends a canonically tagged optional nonce value.
    pub fn push_optional_u64(bytes: &mut Vec<u8>, value: Option<u64>) {
        match value {
            Some(value) => {
                bytes.push(1);
                bytes.extend_from_slice(&value.to_be_bytes());
            }
            None => bytes.extend_from_slice(&[0; 9]),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::hex;
    use alloy_rpc_types_engine::PayloadId;

    use super::*;
    use crate::{BackrunHop, BackrunPlanDigest};

    #[test]
    fn zero_nonce_is_not_rejected_by_witness_policy() {
        let witness = SnapshotNonceWitnessV1::committed(B256::repeat_byte(1), 7, 0);
        assert_eq!(witness.resolve(), Ok(0));
    }

    #[test]
    fn nonce_witness_retains_and_checks_original_current_and_parent() {
        let parent = B256::repeat_byte(2);
        let witness = SnapshotNonceWitnessV1::pending(parent, 8, 4, 4, 7);
        assert_eq!(witness.resolve(), Ok(7));
        assert_eq!(witness.parent_block_hash, parent);
        assert_eq!(witness.parent_block_number, 8);
        assert_eq!(witness.pending_original, Some(4));
        assert_eq!(witness.pending_current, Some(7));

        let incoherent = SnapshotNonceWitnessV1::pending(parent, 8, 4, 3, 7);
        assert_eq!(witness.resolve(), Ok(7));
        assert_eq!(incoherent.resolve(), Err(MeasurementBindingError::NonceWitnessIncoherent));
    }

    fn input() -> MeasurementBindingInputV1 {
        let victim_raw = Bytes::copy_from_slice(&hex!(
            "02f86c8221058034839a4ae283021528942f16386bb37709016023232523ff6d9daf444be380841249c58bc080a001b927eda2af9b00b52a57be0885e0303c39dd2831732e14051c2336470fd468a0681bf120baf562915841a48601c2b54a6742511e535cf8f71c95115af7ff63bd"
        ));
        let parent_hash = B256::repeat_byte(0x11);
        let token = Address::with_last_byte(3);
        let mut plan = BackrunPlan {
            parent_hash,
            block_number: 100,
            predecessor_index: 2,
            payload_id: PayloadId::new([7; 8]),
            victim: keccak256(&victim_raw),
            route: [
                BackrunHop {
                    pool: Address::with_last_byte(1),
                    protocol: ExactProtocol::UniswapV2,
                    token_in: Address::with_last_byte(4),
                    token_out: token,
                    fee_pips: 3_000,
                },
                BackrunHop {
                    pool: Address::with_last_byte(2),
                    protocol: ExactProtocol::UniswapV2,
                    token_in: token,
                    token_out: Address::with_last_byte(4),
                    fee_pips: 3_000,
                },
            ],
            amount_in: U256::from(1_000),
            amount_out: U256::from(1_100),
            gross_profit: U256::from(100),
            digest: BackrunPlanDigest(B256::ZERO),
        };
        plan.digest = MeasurementEncoder::digest(&plan).expect("valid plan");

        MeasurementBindingInputV1 {
            nonce: SnapshotNonceWitnessV1::pending(parent_hash, 99, 4, 4, 7),
            snapshot_base_fee_per_gas: 10,
            plan,
            execution_hops: [
                MeasurementExecutionHopV1 {
                    adapter: Address::with_last_byte(5),
                    min_amount_out: U256::from(1_050),
                    funding_target: Address::with_last_byte(6),
                },
                MeasurementExecutionHopV1 {
                    adapter: Address::with_last_byte(7),
                    min_amount_out: U256::from(1_100),
                    funding_target: Address::with_last_byte(8),
                },
            ],
            victim_raw,
        }
    }

    #[test]
    fn binding_is_deterministic_and_fails_closed_for_mutations() {
        let input = input();
        let victim_raw = input.victim_raw.clone();
        let derived = MeasurementBindingDeriverV1::bind(input).expect("measurement binding");
        assert_eq!(MeasurementBindingDeriverV1::validate(&derived, &victim_raw), Ok(()));

        let mut selected_nonce = derived.clone();
        selected_nonce.selected_nonce += 1;
        assert_eq!(
            MeasurementBindingDeriverV1::validate(&selected_nonce, &victim_raw),
            Err(MeasurementBindingError::BindingMismatch)
        );

        let mut snapshot_base_fee = derived.clone();
        snapshot_base_fee.snapshot_base_fee_per_gas += 1;
        assert_eq!(
            MeasurementBindingDeriverV1::validate(&snapshot_base_fee, &victim_raw),
            Err(MeasurementBindingError::BindingMismatch)
        );

        let mut priority_fee = derived.clone();
        priority_fee.victim_max_priority_fee_per_gas += 1;
        assert_eq!(
            MeasurementBindingDeriverV1::validate(&priority_fee, &victim_raw),
            Err(MeasurementBindingError::BindingMismatch)
        );

        let mut valid_until_block = derived.clone();
        valid_until_block.valid_until_block += 1;
        assert_eq!(
            MeasurementBindingDeriverV1::validate(&valid_until_block, &victim_raw),
            Err(MeasurementBindingError::BindingMismatch)
        );

        let mut execution_hop = derived;
        execution_hop.execution_hops[0].min_amount_out += U256::from(1);
        assert_eq!(
            MeasurementBindingDeriverV1::validate(&execution_hop, &victim_raw),
            Err(MeasurementBindingError::BindingMismatch)
        );
    }
}
