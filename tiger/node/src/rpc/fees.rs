//! Fee methods as base-reth answers them: `eth_feeHistory` (reth's, including the differences
//! between its fee history cache and its database path), `eth_maxPriorityFeePerGas` and
//! `eth_gasPrice` (the OP stack's tip oracle), and `eth_blobBaseFee`.

use alloy_consensus::{BlockHeader, Transaction};
use alloy_eips::{BlockNumberOrTag, eip7840::BlobParams};
use alloy_primitives::{U64, U256};
use alloy_rpc_types_eth::FeeHistory;
use base_common_genesis::BaseUpgrade;
use base_tiger_evm::BlockRules;
use serde_json::{Value, json};

use super::param;
use crate::{Node, RpcError, chain::StoredBlock};

/// reth's `max_header_history` and `max_block_history`: `blockCount` is clamped to this.
const FEE_HISTORY_BLOCKS_MAX: u64 = 1024;
/// At most this many reward percentiles.
const REWARD_PERCENTILES_MAX: usize = 100;
/// Blocks reth's fee history cache keeps.
const FEE_HISTORY_CACHE_BLOCKS: u64 = 1124;
/// The cache precomputes rewards at this many steps per percent.
const FEE_HISTORY_CACHE_RESOLUTION: f64 = 4.0;
/// base-reth's `--min-suggested-priority-fee` default, in wei.
const MIN_SUGGESTED_PRIORITY_FEE: u128 = 1_000_000;
/// reth's gas price oracle cap: 500 gwei.
const MAX_PRICE: u128 = 500_000_000_000;

impl Node {
    /// Answers the fee methods; `None` for other methods.
    pub(super) fn rpc_fees(&self, method: &str, p: &Value) -> Option<Result<Value, RpcError>> {
        Some(match method {
            "eth_feeHistory" => self.fee_history(p),
            "eth_maxPriorityFeePerGas" => Ok(json!(U256::from(self.suggested_tip(&self.head())))),
            "eth_gasPrice" => {
                let head = self.head();
                let base_fee = u128::from(head.header.base_fee_per_gas.unwrap_or_default());
                Ok(json!(U256::from(self.suggested_tip(&head) + base_fee)))
            }
            "eth_blobBaseFee" => {
                let head = self.head();
                let params = self.blob_params(head.header.timestamp);
                head.header
                    .maybe_next_block_blob_fee(params)
                    .map(|fee| json!(U256::from(fee)))
                    .ok_or_else(|| RpcError::internal("excess blob gas missing in the latest block header".into()))
            }
            _ => return None,
        })
    }

    fn head(&self) -> std::sync::Arc<StoredBlock> {
        self.chain.read().expect("chain lock").head()
    }

    /// The blob parameters base-reth's chain spec gives a block at `timestamp`: Ethereum's
    /// schedule with Cancun at Ecotone, Prague at Isthmus and Osaka at Azul.
    pub(super) fn blob_params(&self, timestamp: u64) -> Option<BlobParams> {
        let active = |upgrade| self.spec.is_upgrade_active(upgrade, timestamp);
        if active(BaseUpgrade::Azul) {
            Some(BlobParams::osaka())
        } else if active(BaseUpgrade::Isthmus) {
            Some(BlobParams::prague())
        } else if active(BaseUpgrade::Ecotone) {
            Some(BlobParams::cancun())
        } else {
            None
        }
    }

    /// `eth_feeHistory(blockCount, newestBlock, rewardPercentiles?)`.
    fn fee_history(&self, p: &Value) -> Result<Value, RpcError> {
        let count: U64 = param(p, 0)?;
        let newest: BlockNumberOrTag = param(p, 1)?;
        let percentiles: Option<Vec<f64>> = param(p, 2)?;
        let mut count = count.to::<u64>();
        if count == 0 {
            return Ok(json!(FeeHistory::default()));
        }
        if let Some(percentiles) = &percentiles {
            let invalid = || RpcError::invalid_params("invalid reward percentiles".into());
            if percentiles.len() > REWARD_PERCENTILES_MAX {
                return Err(invalid());
            }
            if percentiles.iter().any(|p| *p < 0.0 || *p > 100.0) || percentiles.windows(2).any(|w| w[0] > w[1]) {
                return Err(invalid());
            }
        }
        count = count.min(FEE_HISTORY_BLOCKS_MAX);
        let chain = self.chain.read().expect("chain lock");
        let best = chain.head().number();
        let end = match newest {
            BlockNumberOrTag::Number(number) if number > best => {
                return Err(RpcError::invalid_params(format!("request beyond head block: requested {number}, head {best}")));
            }
            _ => Self::tag_number(&chain, newest)?,
        };
        count = count.min(end + 1);
        let start = end + 1 - count;
        let blocks: Vec<_> = (start..=end).filter_map(|number| chain.canonical_block(number)).collect();
        drop(chain);
        if blocks.len() as u64 != count {
            return Err(RpcError::invalid_params("invalid block range".into()));
        }
        // reth's cache holds the blocks imported since it started, never genesis.
        let genesis = self.spec.genesis_number;
        let cache_lower = (genesis + 1).max(best.saturating_sub(FEE_HISTORY_CACHE_BLOCKS - 1));
        let cached = start >= cache_lower && best > genesis;
        let mut history = FeeHistory { oldest_block: start, ..FeeHistory::default() };
        for block in &blocks {
            let header = &block.header;
            history.base_fee_per_gas.push(u128::from(header.base_fee_per_gas.unwrap_or_default()));
            history.gas_used_ratio.push(header.gas_used as f64 / header.gas_limit as f64);
            let params = self.blob_params(header.timestamp);
            history.base_fee_per_blob_gas.push(params.and_then(|params| header.blob_fee(params)).unwrap_or_default());
            // The cache takes blob gas from the body, which Base never has; the database path
            // takes the header's, the DA footprint after Jovian.
            history.blob_gas_used_ratio.push(match params {
                Some(params) if !cached => {
                    header.blob_gas_used.unwrap_or_default() as f64 / params.max_blob_gas_per_block() as f64
                }
                _ => 0.0,
            });
        }
        let last = &blocks.last().expect("count is at least one").header;
        let rules = BlockRules { chain: &self.spec };
        history.base_fee_per_gas.push(u128::from(rules.next_base_fee(last, last.timestamp).unwrap_or_default()));
        history
            .base_fee_per_blob_gas
            .push(last.maybe_next_block_blob_fee(self.blob_params(last.timestamp)).unwrap_or_default());
        history.reward = percentiles.map(|percentiles| {
            if cached && percentiles.is_empty() {
                return vec![Vec::new(); blocks.len()];
            }
            let percentiles: Vec<f64> = if cached {
                percentiles
                    .iter()
                    .map(|p| (p * FEE_HISTORY_CACHE_RESOLUTION).round() / FEE_HISTORY_CACHE_RESOLUTION)
                    .collect()
            } else {
                percentiles
            };
            if percentiles.is_empty() {
                return Vec::new();
            }
            blocks.iter().map(|block| Self::reward_percentiles(block, &percentiles)).collect()
        });
        Ok(json!(history))
    }

    /// reth's `calculate_reward_percentiles_for_block`: tips weighted by gas used.
    fn reward_percentiles(block: &StoredBlock, percentiles: &[f64]) -> Vec<u128> {
        let base_fee = block.header.base_fee_per_gas.unwrap_or_default();
        let mut previous = 0;
        let mut transactions: Vec<(u64, u128)> = block
            .receipts
            .iter()
            .zip(&block.transactions)
            .map(|(receipt, tx)| {
                let cumulative = receipt.cumulative_gas_used();
                let gas_used = cumulative - previous;
                previous = cumulative;
                (gas_used, tx.effective_tip_per_gas(base_fee).unwrap_or_default())
            })
            .collect();
        transactions.sort_unstable_by_key(|(_, reward)| *reward);
        let Some(first) = transactions.first() else { return vec![0; percentiles.len()] };
        let mut index = 0;
        let mut cumulative = first.0;
        percentiles
            .iter()
            .map(|percentile| {
                let threshold = (block.header.gas_used as f64 * percentile / 100.0) as u64;
                while cumulative < threshold && index < transactions.len() - 1 {
                    index += 1;
                    cumulative += transactions[index].0;
                }
                transactions[index].1
            })
            .collect()
    }

    /// reth's `op_suggest_tip_cap`: the minimum unless the head block is at capacity, then the
    /// median tip plus 10%, capped at 500 gwei.
    fn suggested_tip(&self, head: &StoredBlock) -> u128 {
        let mut previous = 0;
        let max_tx_gas = head
            .receipts
            .iter()
            .map(|receipt| {
                let cumulative = receipt.cumulative_gas_used();
                let gas = cumulative - previous;
                previous = cumulative;
                gas
            })
            .max()
            .unwrap_or_default();
        let mut suggestion = MIN_SUGGESTED_PRIORITY_FEE;
        if head.header.gas_used + max_tx_gas > head.header.gas_limit {
            let base_fee = head.header.base_fee_per_gas.unwrap_or_default();
            let mut tips: Vec<u128> =
                head.transactions.iter().filter_map(|tx| tx.effective_tip_per_gas(base_fee)).collect();
            if tips.is_empty() {
                return MIN_SUGGESTED_PRIORITY_FEE;
            }
            tips.sort_unstable();
            let middle = tips.len() / 2;
            let median = if tips.len() % 2 == 1 {
                tips[middle]
            } else {
                ((U256::from(tips[middle - 1]) + U256::from(tips[middle])) / U256::from(2)).to::<u128>()
            };
            suggestion = suggestion.max(median.saturating_add(median / 10));
        }
        suggestion.min(MAX_PRICE)
    }
}
