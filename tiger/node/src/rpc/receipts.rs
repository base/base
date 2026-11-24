//! Receipts and logs as base-reth serves them: the Ethereum receipt fields plus Base's L1 fee
//! fields (from the block's L1 attributes deposit), deposit fields, the Jovian DA footprint in
//! `blobGasUsed`, Base transaction fields, and Denim millisecond timestamps.

use alloy_consensus::{Transaction, TxReceipt, transaction::TransactionMeta};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{TxKind, U256};
use alloy_rpc_types_eth::Log;
use base_common_consensus::{BaseReceipt, BaseReceiptEnvelope, BaseTimeUpdateTx};
use base_common_genesis::BaseFeature;
use base_common_l1_fees::L1FeeParams;
use base_common_rpc_types::{BaseLogResponse, BaseTransactionReceipt, L1BlockInfo};
use serde_json::{Value, json};

use crate::{Node, RpcError, chain::StoredBlock};

/// The L1 fee parameters of a block, parsed from the calldata of its first transaction (the L1
/// attributes deposit) exactly as base-reth's `extract_l1_info` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct L1Info;

impl L1Info {
    const ECOTONE_SELECTOR: [u8; 4] = [0x44, 0x0a, 0x5e, 0x20];
    const ISTHMUS_SELECTOR: [u8; 4] = [0x09, 0x89, 0x99, 0xbe];
    const JOVIAN_SELECTOR: [u8; 4] = [0x3d, 0xb6, 0xbe, 0x2b];

    /// Parses the L1 attributes calldata; `None` when it is malformed.
    pub(super) fn parse(input: &[u8]) -> Option<L1FeeParams> {
        if input.len() < 4 {
            return None;
        }
        let (selector, data) = input.split_at(4);
        let word = |range: core::ops::Range<usize>| U256::from_be_slice(&data[range]);
        let mut params = L1FeeParams::default();
        let length = match selector {
            s if s == Self::JOVIAN_SELECTOR => 174,
            s if s == Self::ISTHMUS_SELECTOR => 172,
            s if s == Self::ECOTONE_SELECTOR => 160,
            _ => {
                if data.len() != 256 {
                    return None;
                }
                params.l1_base_fee = word(64..96);
                params.l1_fee_overhead = Some(word(192..224));
                params.l1_base_fee_scalar = word(224..256);
                return Some(params);
            }
        };
        if data.len() != length {
            return None;
        }
        params.l1_base_fee_scalar = word(0..4);
        params.l1_blob_base_fee_scalar = Some(word(4..8));
        params.l1_base_fee = word(32..64);
        params.l1_blob_base_fee = Some(word(64..96));
        if length >= 172 {
            params.operator_fee_scalar = Some(word(160..164));
            params.operator_fee_constant = Some(word(164..172));
        }
        if length == 174 {
            params.da_footprint_gas_scalar = Some(word(172..174));
        }
        Some(params)
    }
}

/// Divisor of the pre-Ecotone L1 fee scalar's 6-decimal fixed point encoding.
const L1_FEE_SCALAR_DIVISOR: f64 = 1_000_000.0;
/// Fixed-point scale of the Fjord FastLZ size estimate.
const FJORD_TX_SIZE_SCALE: u64 = 1_000_000;

impl Node {
    /// The Denim millisecond timestamp of `block`, from its `BaseTime` update deposit.
    pub fn timestamp_ms(block: &StoredBlock) -> Option<u64> {
        BaseTimeUpdateTx::extract_timestamp_ms(&block.transactions, block.number(), block.header.timestamp)
            .ok()
    }

    /// The RPC logs of transaction `index` of `block`.
    pub fn transaction_logs(block: &StoredBlock, index: usize, first_log_index: usize) -> Vec<BaseLogResponse> {
        let timestamp_ms = Self::timestamp_ms(block);
        Log::collect_for_receipt(first_log_index, Self::meta(block, index), block.receipts[index].logs().iter().cloned())
            .into_iter()
            .map(|inner| BaseLogResponse { inner, block_timestamp_ms: timestamp_ms })
            .collect()
    }

    fn meta(block: &StoredBlock, index: usize) -> TransactionMeta {
        TransactionMeta {
            tx_hash: block.tx_hashes[index],
            index: index as u64,
            block_hash: block.hash,
            block_number: block.number(),
            base_fee: block.header.base_fee_per_gas,
            excess_blob_gas: block.header.excess_blob_gas,
            timestamp: block.header.timestamp,
        }
    }

    /// Every receipt of `block` in RPC form. A block without a parsable L1 attributes deposit
    /// fails, except the genesis block, which has no receipts.
    pub fn block_receipts(&self, block: &StoredBlock) -> Result<Vec<Value>, RpcError> {
        if block.transactions.is_empty() {
            return Ok(Vec::new());
        }
        let l1 = block.transactions.first().and_then(|tx| L1Info::parse(tx.input()));
        let Some(l1) = l1 else {
            if block.number() == self.chain.read().expect("chain lock").genesis.header.number {
                return Ok(Vec::new());
            }
            return Err(RpcError::internal("failed to extract l1 block info".into()));
        };
        let mut first_log_index = 0;
        let mut out = Vec::with_capacity(block.transactions.len());
        for index in 0..block.transactions.len() {
            out.push(self.receipt_json(block, index, first_log_index, &l1)?);
            first_log_index += block.receipts[index].logs().len();
        }
        Ok(out)
    }

    /// The RPC receipt of transaction `index` of `block`.
    pub fn transaction_receipt(&self, block: &StoredBlock, index: usize) -> Result<Value, RpcError> {
        let receipts = self.block_receipts(block)?;
        Ok(receipts.into_iter().nth(index).unwrap_or(Value::Null))
    }

    fn receipt_json(
        &self,
        block: &StoredBlock,
        index: usize,
        first_log_index: usize,
        l1: &L1FeeParams,
    ) -> Result<Value, RpcError> {
        let tx = &block.transactions[index];
        let envelope = &block.receipts[index];
        let from = block.senders[index];
        let timestamp = block.header.timestamp;
        let gas_used = envelope.cumulative_gas_used()
            - index.checked_sub(1).map_or(0, |previous| block.receipts[previous].cumulative_gas_used());
        let logs = Self::transaction_logs(block, index, first_log_index);
        let receipt: BaseReceipt = envelope.clone().into();
        let bloom = envelope.bloom();
        let mut logs = logs.into_iter();
        let receipt = receipt.map_logs(|_| logs.next().expect("one RPC log per log"));

        let (mut contract_address, mut to) = match tx.kind() {
            TxKind::Create => (Some(from.create(tx.nonce())), None),
            TxKind::Call(address) => (None, Some(address)),
        };
        if let (Some(_), BaseReceiptEnvelope::Deposit(deposit)) = (contract_address, envelope)
            && let Some(nonce) = deposit.receipt.deposit_nonce
        {
            contract_address = Some(from.create(nonce));
        }
        let base_tx = tx.as_base_tx();
        if base_tx.is_some() {
            to = None;
        }
        let payer = base_tx.and_then(|signed| signed.resolved_payer(from).ok());
        let phase_statuses = match (envelope, base_tx) {
            (BaseReceiptEnvelope::Base(receipt), Some(signed)) => Some(
                receipt
                    .receipt
                    .phase_statuses(signed.tx().calls.len())
                    .map_err(|error| RpcError::internal(error.to_string()))?,
            ),
            _ => None,
        };
        let metadata = base_tx.map(|signed| signed.tx().metadata.clone()).filter(|metadata| !metadata.is_empty());

        let raw = tx.encoded_2718();
        let spec = self.spec.spec_at(timestamp);
        let upgrade = spec.upgrade();
        let blob_gas_used = self.spec.is_active(BaseFeature::DaFootprint, timestamp).then(|| {
            base_common_flz::tx_estimated_size_fjord(&raw)
                .saturating_div(FJORD_TX_SIZE_SCALE)
                .saturating_mul(l1.da_footprint_gas_scalar.unwrap_or_default().saturating_to::<u64>())
        });
        let l1_fee = if tx.is_deposit() { U256::ZERO } else { l1.calculate_tx_l1_cost(&raw, upgrade) };
        let l1_gas_used = L1FeeParams::data_gas(&raw, upgrade).saturating_add(l1.l1_fee_overhead.unwrap_or_default());
        let operator_fee_set = l1.operator_fee_scalar.is_some_and(|value| !value.is_zero())
            || l1.operator_fee_constant.is_some_and(|value| !value.is_zero());
        let l1_block_info = L1BlockInfo {
            l1_gas_price: Some(l1.l1_base_fee.saturating_to()),
            l1_gas_used: Some(l1_gas_used.saturating_to()),
            l1_fee: Some(l1_fee.saturating_to()),
            l1_fee_scalar: (!self.spec.is_active(BaseFeature::BlobAwareL1Fees, timestamp))
                .then(|| f64::from(l1.l1_base_fee_scalar.saturating_to::<u32>()) / L1_FEE_SCALAR_DIVISOR),
            l1_base_fee_scalar: Some(l1.l1_base_fee_scalar.saturating_to()),
            l1_blob_base_fee: l1.l1_blob_base_fee.map(|fee| fee.saturating_to()),
            l1_blob_base_fee_scalar: l1.l1_blob_base_fee_scalar.map(|scalar| scalar.saturating_to()),
            operator_fee_scalar: l1.operator_fee_scalar.filter(|_| operator_fee_set).map(|value| value.saturating_to()),
            operator_fee_constant: l1
                .operator_fee_constant
                .filter(|_| operator_fee_set)
                .map(|value| value.saturating_to()),
            da_footprint_gas_scalar: l1.da_footprint_gas_scalar.map(|value| value.saturating_to()),
        };
        let receipt = BaseTransactionReceipt {
            inner: alloy_rpc_types_eth::TransactionReceipt {
                inner: alloy_consensus::ReceiptWithBloom { receipt, logs_bloom: bloom },
                transaction_hash: block.tx_hashes[index],
                transaction_index: Some(index as u64),
                block_hash: Some(block.hash),
                block_number: Some(block.number()),
                gas_used,
                effective_gas_price: tx.effective_gas_price(block.header.base_fee_per_gas),
                blob_gas_used,
                blob_gas_price: None,
                from,
                to,
                contract_address,
            },
            l1_block_info,
            payer,
            phase_statuses,
            metadata,
        };
        Ok(json!(receipt))
    }
}
