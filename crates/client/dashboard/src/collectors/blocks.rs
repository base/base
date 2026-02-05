//! Block data collector from canonical state subscription.

use alloy_primitives::{Address, B256, Bytes, U256};
use reth_primitives_traits::BlockHeader;

use crate::types::{BlockForWeb, ForkChoiceData, LogForWeb, ReceiptForWeb, TransactionForWeb};

/// Collector for block data from canonical state notifications.
#[derive(Debug, Default)]
pub(crate) struct BlockCollector {
    /// Safe block number (lags behind head).
    safe_block: u64,
    /// Finalized block number.
    finalized_block: u64,
}

impl BlockCollector {
    /// Creates a new block collector.
    pub(crate) const fn new() -> Self {
        Self { safe_block: 0, finalized_block: 0 }
    }

    /// Returns the current safe block number.
    pub(crate) const fn safe_block(&self) -> u64 {
        self.safe_block
    }

    /// Returns the current finalized block number.
    pub(crate) const fn finalized_block(&self) -> u64 {
        self.finalized_block
    }

    /// Updates safe and finalized block numbers.
    pub(crate) const fn update_finality(&mut self, safe: u64, finalized: u64) {
        self.safe_block = safe;
        self.finalized_block = finalized;
    }

    /// Collects fork choice data with full block information.
    #[allow(dead_code)]
    pub(crate) fn collect_fork_choice<H: BlockHeader>(
        &self,
        header: &H,
        block_hash: B256,
        block_size: usize,
        transactions: Vec<TransactionForWeb>,
        receipts: Vec<ReceiptForWeb>,
    ) -> ForkChoiceData {
        let block = BlockForWeb {
            extra_data: format!("0x{}", hex::encode(header.extra_data())),
            gas_limit: format!("0x{:x}", header.gas_limit()),
            gas_used: format!("0x{:x}", header.gas_used()),
            hash: format!("0x{block_hash:x}"),
            beneficiary: format!("0x{:x}", header.beneficiary()),
            number: format!("0x{:x}", header.number()),
            size: format!("0x{block_size:x}"),
            timestamp: format!("0x{:x}", header.timestamp()),
            base_fee_per_gas: header
                .base_fee_per_gas()
                .map(|f| format!("0x{f:x}"))
                .unwrap_or_else(|| "0x0".to_string()),
            blob_gas_used: header
                .blob_gas_used()
                .map(|g| format!("0x{g:x}"))
                .unwrap_or_else(|| "0x0".to_string()),
            excess_blob_gas: header
                .excess_blob_gas()
                .map(|g| format!("0x{g:x}"))
                .unwrap_or_else(|| "0x0".to_string()),
            tx: transactions,
            receipts,
        };

        ForkChoiceData {
            head: block,
            safe: format!("0x{:x}", self.safe_block),
            finalized: format!("0x{:x}", self.finalized_block),
        }
    }
}

/// Converts transaction data to web format.
#[allow(clippy::too_many_arguments)]
pub(crate) fn tx_to_web(
    hash: B256,
    from: Address,
    to: Option<Address>,
    tx_type: u8,
    max_priority_fee: u128,
    max_fee: u128,
    gas_price: u128,
    gas_limit: u64,
    nonce: u64,
    value: U256,
    input: Bytes,
    blob_count: u8,
) -> TransactionForWeb {
    // Extract method selector (first 4 bytes of input)
    let method =
        if input.len() >= 4 { format!("0x{}", hex::encode(&input[..4])) } else { "0x".to_string() };

    TransactionForWeb {
        hash: format!("0x{hash:x}"),
        from: format!("0x{from:x}"),
        to: to.map(|a| format!("0x{a:x}")).unwrap_or_default(),
        tx_type,
        max_priority_fee_per_gas: format!("0x{max_priority_fee:x}"),
        max_fee_per_gas: format!("0x{max_fee:x}"),
        gas_price: format!("0x{gas_price:x}"),
        gas_limit: format!("0x{gas_limit:x}"),
        nonce: format!("0x{nonce:x}"),
        value: format!("0x{value:x}"),
        data_length: input.len(),
        blobs: blob_count,
        method,
    }
}

/// Converts receipt data to web format.
pub(crate) fn receipt_to_web(
    gas_used: u64,
    effective_gas_price: u128,
    contract_address: Option<Address>,
    blob_gas_price: Option<u128>,
    blob_gas_used: Option<u64>,
    logs: Vec<(Address, Bytes, Vec<B256>)>,
    success: bool,
) -> ReceiptForWeb {
    ReceiptForWeb {
        gas_used: format!("0x{gas_used:x}"),
        effective_gas_price: format!("0x{effective_gas_price:x}"),
        contract_address: contract_address.map(|a| format!("0x{a:x}")).unwrap_or_default(),
        blob_gas_price: blob_gas_price
            .map(|p| format!("0x{p:x}"))
            .unwrap_or_else(|| "0x0".to_string()),
        blob_gas_used: blob_gas_used
            .map(|u| format!("0x{u:x}"))
            .unwrap_or_else(|| "0x0".to_string()),
        logs: logs
            .into_iter()
            .map(|(addr, data, topics)| LogForWeb {
                address: format!("0x{addr:x}"),
                data: format!("0x{}", hex::encode(&data)),
                topics: topics.iter().map(|t| format!("0x{t:x}")).collect(),
            })
            .collect(),
        status: if success { "0x1".to_string() } else { "0x0".to_string() },
    }
}
