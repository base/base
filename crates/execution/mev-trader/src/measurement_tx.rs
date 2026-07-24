//! Pure derivation of the non-broadcastable edge measurement transaction.

use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope, private::alloy_rlp::Decodable};
use alloy_eips::{Decodable2718, eip2930::AccessList};
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, address, keccak256};
use thiserror::Error;

use crate::{BackrunPlan, ExactProtocol, MeasurementEncoder};

/// Base mainnet chain identifier pinned by the measurement contract.
pub const MEASUREMENT_CHAIN_ID: u64 = 8_453;
/// Fixed gas limit for the measurement-only executor call.
pub const MEASUREMENT_GAS_LIMIT: u64 = 3_000_000;
/// Pinned Blink atomic executor used by the offline measurement envelope.
pub const MEASUREMENT_EXECUTOR: Address = address!("1810cbFA042e8199121021F056Afe8B31028CF55");

/// Hash-pinned snapshot-local committed and pending nonce evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeasurementNonceWitnessV1 {
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

impl MeasurementNonceWitnessV1 {
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

/// Per-hop executor-only evidence absent from the selected measurement plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeasurementExecutionHopV1 {
    /// Validated adapter address.
    pub adapter: Address,
    /// Exact output floor for this hop.
    pub min_amount_out: U256,
    /// Validated funding target.
    pub funding_target: Address,
}

/// Complete preloaded inputs for one pure unsigned derivation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeasurementTxInputV1 {
    /// Snapshot-local nonce evidence.
    pub nonce: MeasurementNonceWitnessV1,
    /// Base fee from the captured pending snapshot header.
    pub snapshot_base_fee_per_gas: u128,
    /// Selected same-frame two-hop plan.
    pub plan: BackrunPlan,
    /// Executor-only evidence validated by the edge owner.
    pub execution_hops: [MeasurementExecutionHopV1; 2],
    /// Exact signed victim EIP-2718 bytes.
    pub victim_raw_tx: Bytes,
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
    /// Exact locally encoded executor ABI calldata.
    pub calldata: Bytes,
    /// Full hash-pinned nonce witness, including original and current overlay values.
    pub nonce_witness: MeasurementNonceWitnessV1,
    /// Selected nonce retained separately for mutation detection.
    pub selected_nonce: u64,
    /// Snapshot base fee used by the checked fee rule.
    pub snapshot_base_fee_per_gas: u128,
    /// Candidate block plus the pinned zero-block validity window.
    pub valid_until_block: u64,
    /// Victim transaction hash bound into the candidate.
    pub target_tx_hash: B256,
    /// Exact parsed victim EIP-1559 literals.
    pub victim_transaction: TxEip1559,
    /// Exact signed victim envelope bytes retained for byte parity.
    pub victim_raw_tx: Bytes,
    /// Same-frame selected plan retained for query-free revalidation.
    pub plan: BackrunPlan,
    /// Same-frame executor evidence retained for query-free calldata revalidation.
    pub execution_hops: [MeasurementExecutionHopV1; 2],
}

/// Fail-closed errors from pure measurement transaction derivation.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum MeasurementTxError {
    /// Committed and pending nonce evidence was incomplete or incoherent.
    #[error("nonce witness is incoherent")]
    NonceWitnessIncoherent,
    /// The selected plan or its executor evidence was invalid.
    #[error("selected measurement plan or executor evidence is invalid")]
    InvalidExecutionPlan,
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
    /// The victim was not an exact signed EIP-1559 envelope bound to the plan.
    #[error("victim envelope is not exact EIP-1559 plan evidence")]
    VictimEnvelopeMismatch,
    /// Locally encoded executor calldata failed exact reparse.
    #[error("measurement calldata reparse mismatch")]
    MeasurementCalldataReparseMismatch,
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
        MeasurementEncoder::validate(&input.plan)
            .map_err(|_| MeasurementTxError::InvalidExecutionPlan)?;
        let nonce = input.nonce.resolve()?;
        if input.nonce.parent_block_hash != input.plan.parent_hash
            || input.nonce.parent_block_number.checked_add(1) != Some(input.plan.block_number)
        {
            return Err(MeasurementTxError::NonceWitnessIncoherent);
        }
        let (victim_transaction, victim_hash) = Self::parse_victim(&input.victim_raw_tx)?;
        let victim_max_fee_per_gas = victim_transaction.max_fee_per_gas;
        let victim_priority = victim_transaction.max_priority_fee_per_gas;
        if victim_hash != input.plan.victim {
            return Err(MeasurementTxError::VictimEnvelopeMismatch);
        }
        let max_fee_per_gas = input
            .snapshot_base_fee_per_gas
            .checked_add(victim_priority)
            .ok_or(MeasurementTxError::MaxFeeOverflow)?;
        if max_fee_per_gas > victim_max_fee_per_gas {
            return Err(MeasurementTxError::MaxFeeExceedsVictim);
        }
        let calldata =
            Self::encode_calldata(&input.plan, input.execution_hops, input.plan.block_number)?;
        Self::reparse_calldata(
            &calldata,
            &input.plan,
            input.execution_hops,
            input.plan.block_number,
        )?;

        let transaction = TxEip1559 {
            chain_id: MEASUREMENT_CHAIN_ID,
            nonce,
            gas_limit: MEASUREMENT_GAS_LIMIT,
            max_fee_per_gas,
            max_priority_fee_per_gas: victim_priority,
            to: TxKind::Call(MEASUREMENT_EXECUTOR),
            value: U256::ZERO,
            access_list: AccessList::default(),
            input: calldata.clone(),
        };
        let encoded = transaction.encoded_for_signing();
        let envelope_hash = keccak256(&encoded);
        let mut rlp_fields = encoded.get(1..).unwrap_or_default();
        let verification = TxEip1559::decode(&mut rlp_fields)
            .map_err(|_| MeasurementTxError::MeasurementEnvelopeReparseMismatch)?;
        if encoded.first() != Some(&2)
            || !rlp_fields.is_empty()
            || verification != transaction
            || verification.encoded_for_signing() != encoded
            || keccak256(&encoded) != envelope_hash
        {
            return Err(MeasurementTxError::MeasurementEnvelopeReparseMismatch);
        }

        Ok(BackrunMeasurementTxV1 {
            transaction,
            unsigned_envelope_bytes: encoded.into(),
            unsigned_envelope_hash: envelope_hash,
            calldata,
            nonce_witness: input.nonce,
            selected_nonce: nonce,
            snapshot_base_fee_per_gas: input.snapshot_base_fee_per_gas,
            valid_until_block: input.plan.block_number,
            target_tx_hash: victim_hash,
            victim_transaction,
            victim_raw_tx: input.victim_raw_tx,
            plan: input.plan,
            execution_hops: input.execution_hops,
        })
    }

    /// Revalidates every retained unsigned and victim envelope literal without external state.
    pub fn validate(transaction: &BackrunMeasurementTxV1) -> Result<(), MeasurementTxError> {
        MeasurementEncoder::validate(&transaction.plan)
            .map_err(|_| MeasurementTxError::InvalidExecutionPlan)?;
        let selected_nonce = transaction.nonce_witness.resolve()?;
        if transaction.nonce_witness.parent_block_hash != transaction.plan.parent_hash
            || transaction.nonce_witness.parent_block_number.checked_add(1)
                != Some(transaction.plan.block_number)
        {
            return Err(MeasurementTxError::NonceWitnessIncoherent);
        }

        let (victim, victim_hash) = Self::parse_victim(&transaction.victim_raw_tx)?;
        if victim != transaction.victim_transaction
            || victim_hash != transaction.target_tx_hash
            || victim_hash != transaction.plan.victim
        {
            return Err(MeasurementTxError::VictimEnvelopeMismatch);
        }

        let max_fee_per_gas = transaction
            .snapshot_base_fee_per_gas
            .checked_add(victim.max_priority_fee_per_gas)
            .ok_or(MeasurementTxError::MaxFeeOverflow)?;
        if max_fee_per_gas > victim.max_fee_per_gas {
            return Err(MeasurementTxError::MaxFeeExceedsVictim);
        }
        let calldata = Self::encode_calldata(
            &transaction.plan,
            transaction.execution_hops,
            transaction.plan.block_number,
        )?;
        Self::reparse_calldata(
            &transaction.calldata,
            &transaction.plan,
            transaction.execution_hops,
            transaction.plan.block_number,
        )?;

        let expected_transaction = TxEip1559 {
            chain_id: MEASUREMENT_CHAIN_ID,
            nonce: selected_nonce,
            gas_limit: MEASUREMENT_GAS_LIMIT,
            max_fee_per_gas,
            max_priority_fee_per_gas: victim.max_priority_fee_per_gas,
            to: TxKind::Call(MEASUREMENT_EXECUTOR),
            value: U256::ZERO,
            access_list: AccessList::default(),
            input: calldata,
        };
        let encoded = expected_transaction.encoded_for_signing();
        if transaction.selected_nonce != selected_nonce
            || transaction.valid_until_block != transaction.plan.block_number
            || transaction.transaction != expected_transaction
            || transaction.transaction.input != transaction.calldata
            || encoded.as_slice() != transaction.unsigned_envelope_bytes.as_ref()
            || keccak256(&encoded) != transaction.unsigned_envelope_hash
        {
            return Err(MeasurementTxError::MeasurementEnvelopeReparseMismatch);
        }
        Ok(())
    }

    fn parse_victim(raw: &[u8]) -> Result<(TxEip1559, B256), MeasurementTxError> {
        if raw.first() != Some(&2) {
            return Err(MeasurementTxError::VictimEnvelopeMismatch);
        }
        let mut bytes = raw;
        let envelope = TxEnvelope::decode_2718(&mut bytes)
            .map_err(|_| MeasurementTxError::VictimEnvelopeMismatch)?;
        if !bytes.is_empty() {
            return Err(MeasurementTxError::VictimEnvelopeMismatch);
        }
        match envelope {
            TxEnvelope::Eip1559(signed) => Ok((signed.tx().clone(), keccak256(raw))),
            _ => Err(MeasurementTxError::VictimEnvelopeMismatch),
        }
    }

    fn encode_calldata(
        plan: &BackrunPlan,
        execution: [MeasurementExecutionHopV1; 2],
        valid_until_block: u64,
    ) -> Result<Bytes, MeasurementTxError> {
        let signature = b"executeBlinkOfaAtomic((address,address,address,address,uint24,uint256,address),(address,address,address,address,uint24,uint256,address),uint256,uint256,uint256)";
        let selector = keccak256(signature);
        let mut bytes = Vec::with_capacity(4 + 17 * 32);
        bytes.extend_from_slice(&selector[..4]);
        for (hop, extra) in plan.route.iter().zip(execution) {
            if extra.adapter.is_zero()
                || extra.funding_target.is_zero()
                || hop.pool.is_zero()
                || hop.token_in.is_zero()
                || hop.token_out.is_zero()
                || hop.token_in == hop.token_out
            {
                return Err(MeasurementTxError::InvalidExecutionPlan);
            }
            Self::push_address(&mut bytes, extra.adapter);
            Self::push_address(&mut bytes, hop.pool);
            Self::push_address(&mut bytes, hop.token_in);
            Self::push_address(&mut bytes, hop.token_out);
            Self::push_u256(&mut bytes, U256::from(Self::fee_bps(hop.protocol, hop.fee_pips)?));
            Self::push_u256(&mut bytes, extra.min_amount_out);
            Self::push_address(&mut bytes, extra.funding_target);
        }
        Self::push_u256(&mut bytes, plan.amount_in);
        Self::push_u256(&mut bytes, plan.amount_out);
        Self::push_u256(&mut bytes, U256::from(valid_until_block));
        Ok(bytes.into())
    }

    fn reparse_calldata(
        calldata: &[u8],
        plan: &BackrunPlan,
        execution: [MeasurementExecutionHopV1; 2],
        valid_until_block: u64,
    ) -> Result<(), MeasurementTxError> {
        let expected = Self::encode_calldata(plan, execution, valid_until_block)?;
        if calldata != expected.as_ref() || calldata.len() != 4 + 17 * 32 {
            return Err(MeasurementTxError::MeasurementCalldataReparseMismatch);
        }
        Ok(())
    }

    const fn fee_bps(protocol: ExactProtocol, fee_pips: u32) -> Result<u32, MeasurementTxError> {
        match protocol {
            ExactProtocol::UniswapV2 | ExactProtocol::AerodromeVolatile => {
                if !fee_pips.is_multiple_of(100) {
                    return Err(MeasurementTxError::InvalidExecutionPlan);
                }
                Ok(fee_pips / 100)
            }
            ExactProtocol::UniswapV3 | ExactProtocol::AerodromeStable => Ok(0),
        }
    }

    fn push_address(bytes: &mut Vec<u8>, address: Address) {
        bytes.extend_from_slice(&[0; 12]);
        bytes.extend_from_slice(address.as_slice());
    }

    fn push_u256(bytes: &mut Vec<u8>, value: U256) {
        bytes.extend_from_slice(&value.to_be_bytes::<32>());
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::Signed;
    use alloy_primitives::Signature;
    use alloy_rpc_types_engine::PayloadId;

    use super::*;
    use crate::{BackrunHop, BackrunPlanDigest};

    #[test]
    fn zero_nonce_and_zero_priority_are_not_rejected_by_witness_policy() {
        let witness = MeasurementNonceWitnessV1::committed(B256::repeat_byte(1), 7, 0);
        assert_eq!(witness.resolve(), Ok(0));
    }

    #[test]
    fn nonce_witness_retains_and_checks_original_current_and_parent() {
        let parent = B256::repeat_byte(2);
        let witness = MeasurementNonceWitnessV1::pending(parent, 8, 4, 4, 7);
        assert_eq!(witness.resolve(), Ok(7));
        assert_eq!(witness.parent_block_hash, parent);
        assert_eq!(witness.parent_block_number, 8);
        assert_eq!(witness.pending_original, Some(4));
        assert_eq!(witness.pending_current, Some(7));

        let incoherent = MeasurementNonceWitnessV1::pending(parent, 8, 4, 3, 7);
        assert_eq!(incoherent.resolve(), Err(MeasurementTxError::NonceWitnessIncoherent));
    }

    fn input() -> MeasurementTxInputV1 {
        let victim = TxEip1559 {
            chain_id: MEASUREMENT_CHAIN_ID,
            nonce: 91,
            gas_limit: 120_000,
            max_fee_per_gas: 30,
            max_priority_fee_per_gas: 2,
            to: TxKind::Call(Address::with_last_byte(9)),
            value: U256::from(3),
            access_list: AccessList::default(),
            input: Bytes::from_static(b"victim"),
        };
        let signed = Signed::new_unchecked(
            victim,
            Signature::new(U256::from(1), U256::from(2), false),
            B256::ZERO,
        );
        let mut victim_raw = vec![2];
        signed.rlp_encode(&mut victim_raw);
        let victim_raw_tx = Bytes::from(victim_raw);

        let parent_hash = B256::repeat_byte(0x11);
        let token = Address::with_last_byte(3);
        let mut plan = BackrunPlan {
            parent_hash,
            block_number: 100,
            predecessor_index: 2,
            payload_id: PayloadId::new([7; 8]),
            victim: keccak256(&victim_raw_tx),
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

        MeasurementTxInputV1 {
            nonce: MeasurementNonceWitnessV1::pending(parent_hash, 99, 4, 4, 7),
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
            victim_raw_tx,
        }
    }

    #[test]
    fn validate_fails_closed_for_mutated_retained_authoritative_literals() {
        let derived = MeasurementTxDeriverV1::derive(input()).expect("measurement transaction");
        assert_eq!(MeasurementTxDeriverV1::validate(&derived), Ok(()));

        let mut selected_nonce = derived.clone();
        selected_nonce.selected_nonce += 1;
        assert_eq!(
            MeasurementTxDeriverV1::validate(&selected_nonce),
            Err(MeasurementTxError::MeasurementEnvelopeReparseMismatch)
        );

        let mut snapshot_base_fee = derived.clone();
        snapshot_base_fee.snapshot_base_fee_per_gas += 1;
        assert_eq!(
            MeasurementTxDeriverV1::validate(&snapshot_base_fee),
            Err(MeasurementTxError::MeasurementEnvelopeReparseMismatch)
        );

        let mut priority_fee = derived.clone();
        priority_fee.transaction.max_priority_fee_per_gas += 1;
        assert_eq!(
            MeasurementTxDeriverV1::validate(&priority_fee),
            Err(MeasurementTxError::MeasurementEnvelopeReparseMismatch)
        );

        let mut max_fee = derived.clone();
        max_fee.transaction.max_fee_per_gas += 1;
        assert_eq!(
            MeasurementTxDeriverV1::validate(&max_fee),
            Err(MeasurementTxError::MeasurementEnvelopeReparseMismatch)
        );

        let mut valid_until_block = derived.clone();
        valid_until_block.valid_until_block += 1;
        assert_eq!(
            MeasurementTxDeriverV1::validate(&valid_until_block),
            Err(MeasurementTxError::MeasurementEnvelopeReparseMismatch)
        );

        let mut nonce_witness = derived.clone();
        nonce_witness.nonce_witness.pending_current = Some(8);
        assert_eq!(
            MeasurementTxDeriverV1::validate(&nonce_witness),
            Err(MeasurementTxError::MeasurementEnvelopeReparseMismatch)
        );

        let mut execution_hop = derived;
        execution_hop.execution_hops[0].min_amount_out += U256::from(1);
        assert_eq!(
            MeasurementTxDeriverV1::validate(&execution_hop),
            Err(MeasurementTxError::MeasurementCalldataReparseMismatch)
        );
    }
}
