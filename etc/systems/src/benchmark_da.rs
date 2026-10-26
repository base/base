//! Builder-equivalent FastLZ/Fjord DA estimates, not L1 batcher or blob bytes.

use alloy_eips::{Decodable2718, Encodable2718};
use base_common_consensus::BaseTxEnvelope;
use base_common_flz::tx_estimated_size_fjord_bytes;
use eyre::Result;
use serde::{Deserialize, Serialize};

/// Optional per-block totals. Missing fields mean unmeasured, not zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockDaMetrics {
    /// Sum of the builder's FastLZ/Fjord estimated bytes, excluding deposits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub da_bytes_estimated: Option<u64>,
    /// Sum of EIP-2718 encoded lengths, excluding deposits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tx_bytes_raw: Option<u64>,
    /// Number of transactions included in both byte totals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub non_deposit_transaction_count: Option<u64>,
}

impl BlockDaMetrics {
    /// Uses the same encoded bytes and estimator as the payload builder's pooled transactions.
    pub fn from_transactions(transactions: impl IntoIterator<Item = BaseTxEnvelope>) -> Self {
        let mut estimated = 0;
        let mut raw = 0;
        let mut count = 0;
        for tx in transactions {
            if matches!(tx, BaseTxEnvelope::Deposit(_)) {
                continue;
            }
            let encoded = tx.encoded_2718();
            estimated += tx_estimated_size_fjord_bytes(&encoded);
            raw += encoded.len() as u64;
            count += 1;
        }
        Self {
            da_bytes_estimated: Some(estimated),
            tx_bytes_raw: Some(raw),
            non_deposit_transaction_count: Some(count),
        }
    }

    /// Decodes already exported payload transactions; callers run this outside replay timing.
    pub fn from_encoded_transactions<'a>(
        transactions: impl IntoIterator<Item = &'a alloy_primitives::Bytes>,
    ) -> Result<Self> {
        let decoded = transactions
            .into_iter()
            .map(|bytes| BaseTxEnvelope::decode_2718_exact(bytes))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Self::from_transactions(decoded))
    }
}

/// Distribution and rates for one kind of bytes across a complete block window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ByteUsageSummary {
    /// Total bytes in the window.
    pub total: u64,
    /// Arithmetic mean bytes per block, including empty blocks.
    pub mean_per_block: f64,
    /// Nearest-rank median bytes per block.
    pub p50_per_block: u64,
    /// Nearest-rank 99th percentile bytes per block.
    pub p99_per_block: u64,
    /// Largest per-block total.
    pub max_per_block: u64,
    /// Total divided by window duration, not replay wall time.
    pub per_second: f64,
    /// Total divided by non-deposit transaction count; absent with no such transactions.
    pub per_non_deposit_tx: Option<f64>,
}

impl ByteUsageSummary {
    fn new(mut samples: Vec<u64>, seconds: f64, transactions: u64) -> Self {
        samples.sort_unstable();
        let total = samples.iter().sum();
        let rank = |percent: usize| samples[(samples.len() * percent).div_ceil(100) - 1];
        Self {
            total,
            mean_per_block: total as f64 / samples.len() as f64,
            p50_per_block: rank(50),
            p99_per_block: rank(99),
            max_per_block: samples[samples.len() - 1],
            per_second: total as f64 / seconds,
            per_non_deposit_tx: (transactions != 0).then(|| total as f64 / transactions as f64),
        }
    }
}

/// DA estimate and raw-byte statistics; neither describes L1 batcher or blob output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaUsageSummary {
    /// Identifies the estimate explicitly as `fastlz_fjord`, not compressed batch/blob bytes.
    pub estimator: String,
    /// Blocks in the measured snapshot window or successfully inserted replay range.
    pub block_count: u64,
    /// Block count times configured cadence, including empty blocks.
    pub measurement_seconds: f64,
    /// Non-deposit transactions in this window.
    pub non_deposit_transaction_count: u64,
    /// Builder-equivalent FastLZ/Fjord DA estimate statistics.
    pub da_bytes_estimated: ByteUsageSummary,
    /// EIP-2718 encoded transaction byte statistics.
    pub tx_bytes_raw: ByteUsageSummary,
}

impl DaUsageSummary {
    /// Returns no summary for an empty, zero-duration, or incompletely measured window.
    pub fn from_blocks<'a>(
        blocks: impl IntoIterator<Item = &'a BlockDaMetrics>,
        block_interval_ms: u64,
    ) -> Option<Self> {
        let blocks = blocks.into_iter().collect::<Vec<_>>();
        if blocks.is_empty() || block_interval_ms == 0 {
            return None;
        }
        let estimated =
            blocks.iter().map(|block| block.da_bytes_estimated).collect::<Option<Vec<_>>>()?;
        let raw = blocks.iter().map(|block| block.tx_bytes_raw).collect::<Option<Vec<_>>>()?;
        let count = blocks
            .iter()
            .map(|block| block.non_deposit_transaction_count)
            .collect::<Option<Vec<_>>>()?
            .into_iter()
            .sum();
        let seconds = blocks.len() as f64 * block_interval_ms as f64 / 1_000.0;
        Some(Self {
            estimator: "fastlz_fjord".into(),
            block_count: blocks.len() as u64,
            measurement_seconds: seconds,
            non_deposit_transaction_count: count,
            da_bytes_estimated: ByteUsageSummary::new(estimated, seconds, count),
            tx_bytes_raw: ByteUsageSummary::new(raw, seconds, count),
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{SignableTransaction, TxEip1559};
    use alloy_primitives::{Address, Sealable, Signature, TxKind, U256};
    use base_common_consensus::TxDeposit;

    use super::*;

    fn transfer() -> BaseTxEnvelope {
        TxEip1559 {
            chain_id: 8453,
            nonce: 0,
            gas_limit: 21_000,
            max_fee_per_gas: 1,
            max_priority_fee_per_gas: 1,
            to: TxKind::Call(Address::repeat_byte(0x22)),
            value: U256::from(7),
            ..Default::default()
        }
        .into_signed(Signature::test_signature())
        .into()
    }

    #[test]
    fn exact_transaction_bytes_exclude_deposits() {
        let deposit = BaseTxEnvelope::Deposit(TxDeposit::default().seal_slow());
        let tx = transfer();
        let encoded = tx.encoded_2718();
        let measured = BlockDaMetrics::from_transactions([deposit.clone(), tx.clone(), tx.clone()]);
        assert_eq!(
            measured.da_bytes_estimated,
            Some(200),
            "two transfers hit Fjord's 100-byte minimum each"
        );
        assert_eq!(encoded.len(), 103, "canonical EIP-1559 fixture length including type byte");
        assert_eq!(measured.tx_bytes_raw, Some(206), "two 103-byte transfer envelopes");
        assert_eq!(
            measured.non_deposit_transaction_count,
            Some(2),
            "deposit excluded from denominator"
        );
        let encoded_txs = [deposit.encoded_2718().into(), encoded.clone().into(), encoded.into()];
        assert_eq!(
            BlockDaMetrics::from_encoded_transactions(&encoded_txs).unwrap(),
            measured,
            "RPC and replay payload accounting must agree exactly"
        );
        assert_eq!(
            BlockDaMetrics::from_transactions([]).da_bytes_estimated,
            Some(0),
            "empty block measured as zero"
        );
    }

    #[test]
    fn exact_window_statistics_and_missing_data() {
        let blocks = [
            BlockDaMetrics {
                da_bytes_estimated: Some(100),
                tx_bytes_raw: Some(200),
                non_deposit_transaction_count: Some(1),
            },
            BlockDaMetrics {
                da_bytes_estimated: Some(300),
                tx_bytes_raw: Some(600),
                non_deposit_transaction_count: Some(3),
            },
        ];
        let summary = DaUsageSummary::from_blocks(&blocks, 200).expect("complete window");
        assert_eq!(summary.estimator, "fastlz_fjord", "explicit estimator label");
        assert_eq!(summary.block_count, 2, "both blocks included");
        assert_eq!(summary.measurement_seconds, 0.4, "use block cadence, not replay wall time");
        assert_eq!(summary.non_deposit_transaction_count, 4, "non-deposit denominator");
        assert_eq!(
            summary.da_bytes_estimated,
            ByteUsageSummary {
                total: 400,
                mean_per_block: 200.0,
                p50_per_block: 100,
                p99_per_block: 300,
                max_per_block: 300,
                per_second: 1000.0,
                per_non_deposit_tx: Some(100.0),
            },
            "exact estimated-byte distribution and rates"
        );
        assert_eq!(
            summary.tx_bytes_raw,
            ByteUsageSummary {
                total: 800,
                mean_per_block: 400.0,
                p50_per_block: 200,
                p99_per_block: 600,
                max_per_block: 600,
                per_second: 2000.0,
                per_non_deposit_tx: Some(200.0),
            },
            "exact raw-byte distribution and rates"
        );
        assert!(
            DaUsageSummary::from_blocks([&blocks[0], &BlockDaMetrics::default()], 200).is_none(),
            "do not silently undercount old or partial data"
        );
        assert!(DaUsageSummary::from_blocks([], 200).is_none(), "empty window has no summary");
        assert!(
            DaUsageSummary::from_blocks(&blocks, 0).is_none(),
            "zero cadence cannot yield a rate"
        );
        let empty = BlockDaMetrics::from_transactions([]);
        assert_eq!(
            DaUsageSummary::from_blocks([&empty], 200).unwrap().tx_bytes_raw.per_non_deposit_tx,
            None,
            "no division by zero"
        );
    }

    #[test]
    fn old_block_json_defaults_to_unmeasured() {
        let block: BlockDaMetrics = serde_json::from_str("{}").expect("old JSON remains readable");
        assert_eq!(block, BlockDaMetrics::default(), "missing fields are unknown, not zero");
        assert_eq!(
            serde_json::to_value(block).unwrap(),
            serde_json::json!({}),
            "unknown fields stay absent"
        );
    }
}
