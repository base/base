//! Header and block rules: what makes a Base block valid against its parent, before and after
//! execution. Mirrors `crates/execution/consensus` and the engine validator in base/base.

use alloy_consensus::{EMPTY_OMMER_ROOT_HASH, Header};
use alloy_eips::{
    calc_next_block_base_fee, eip1559::BaseFeeParams, eip7685::EMPTY_REQUESTS_HASH,
};
use alloy_primitives::{B64, B256, Bloom};
use alloy_trie::EMPTY_ROOT_HASH;
use base_common_consensus::{BaseTimeUpdateTx, BaseTxEnvelope, HoloceneExtraData, JovianExtraData};
use base_common_genesis::BaseFeature;

use crate::ChainSpec;

/// Largest extra data a header may carry.
pub const MAXIMUM_EXTRA_DATA_SIZE: usize = 32;
/// Largest gas limit a header may carry (EIP-1985).
pub const MAXIMUM_GAS_LIMIT: u64 = i64::MAX as u64;

/// Why a block is invalid. The text is for logs and Engine API `validationError`.
pub type Invalid = String;

/// Block rules for one chain.
#[derive(Clone, Copy, Debug)]
pub struct BlockRules<'a> {
    /// The chain.
    pub chain: &'a ChainSpec,
}

impl BlockRules<'_> {
    /// The base fee the chain derives for a block at `timestamp` on top of `parent`, or `None`
    /// when the parent's extra data does not decode (base-reth reports no base fee then).
    pub fn next_base_fee(&self, parent: &Header, timestamp: u64) -> Option<u64> {
        let chain = self.chain;
        let parent_base_fee = parent.base_fee_per_gas.unwrap_or_default();
        if chain.is_active(BaseFeature::DaAwareBaseFee, parent.timestamp) {
            let (elasticity, denominator, min_base_fee) =
                JovianExtraData::decode(&parent.extra_data).ok()?;
            let params = self.params_from_extra_data(timestamp, elasticity, denominator)?;
            // From Jovian the DA footprint counts against the target too.
            let gas_used = parent.gas_used.max(parent.blob_gas_used.unwrap_or_default());
            let next = calc_next_block_base_fee(gas_used, parent.gas_limit, parent_base_fee, params);
            return Some(next.max(min_base_fee));
        }
        if chain.is_active(BaseFeature::DynamicBaseFeeParameters, parent.timestamp) {
            let (elasticity, denominator) = HoloceneExtraData::decode(&parent.extra_data).ok()?;
            let params = self.params_from_extra_data(timestamp, elasticity, denominator)?;
            // base-reth falls back to zero when the parent has no base fee here.
            return Some(calc_next_block_base_fee(
                parent.gas_used,
                parent.gas_limit,
                parent_base_fee,
                params,
            ));
        }
        let params = chain.base_fee_params(timestamp);
        parent.base_fee_per_gas.map(|base_fee| {
            calc_next_block_base_fee(parent.gas_used, parent.gas_limit, base_fee, params)
        })
    }

    /// EIP-1559 parameters from Holocene extra data: zero for both means the chain default.
    fn params_from_extra_data(
        &self,
        timestamp: u64,
        elasticity: u32,
        denominator: u32,
    ) -> Option<BaseFeeParams> {
        match (elasticity, denominator) {
            (0, 0) => Some(self.chain.base_fee_params(timestamp)),
            (0, _) | (_, 0) => None,
            _ => Some(BaseFeeParams::new(u128::from(denominator), u128::from(elasticity))),
        }
    }

    /// Checks of a header on its own.
    pub fn validate_header(&self, header: &Header) -> Result<(), Invalid> {
        let chain = self.chain;
        if header.nonce != B64::ZERO {
            return Err("merge nonce is not zero".into());
        }
        if header.ommers_hash != EMPTY_OMMER_ROOT_HASH {
            return Err("merge ommers hash is not empty".into());
        }
        if header.extra_data.len() > MAXIMUM_EXTRA_DATA_SIZE {
            return Err(format!("extra data is {} bytes", header.extra_data.len()));
        }
        if header.gas_used > header.gas_limit {
            return Err(format!(
                "gas used {} exceeds gas limit {}",
                header.gas_used, header.gas_limit
            ));
        }
        if header.gas_limit > MAXIMUM_GAS_LIMIT {
            return Err(format!("gas limit {} is above the maximum", header.gas_limit));
        }
        if header.base_fee_per_gas.is_none() {
            return Err("base fee missing".into());
        }

        let timestamp = header.timestamp;
        let params = if chain.is_active(BaseFeature::MinBaseFee, timestamp) {
            let (elasticity, denominator, _) = JovianExtraData::decode(&header.extra_data)
                .map_err(|error| format!("extra data: {error}"))?;
            Some((elasticity, denominator))
        } else if chain.is_active(BaseFeature::DynamicBaseFeeParameters, timestamp) {
            Some(
                HoloceneExtraData::decode(&header.extra_data)
                    .map_err(|error| format!("extra data: {error}"))?,
            )
        } else {
            None
        };
        if let Some((elasticity, denominator)) = params
            && (elasticity == 0 || denominator == 0)
        {
            return Err("extra data EIP-1559 parameters are zero".into());
        }

        // The Denim schedule fixes the timestamp by block number, not by the timestamp claimed.
        if let Some(schedule) = chain.denim_schedule()
            && header.number != schedule.genesis_block_number
            && schedule.is_subsecond_active_at_block(header.number)
        {
            let (expected, _) = schedule.block_timestamp_parts(header.number);
            if header.timestamp != expected {
                return Err(format!(
                    "timestamp {} is not the scheduled {expected}",
                    header.timestamp
                ));
            }
        }

        if chain.is_active(BaseFeature::EmptyRequestsHash, timestamp) {
            match header.requests_hash {
                None => return Err("requests hash missing".into()),
                Some(hash) if hash != EMPTY_REQUESTS_HASH => {
                    return Err(format!("requests hash {hash} is not empty"));
                }
                Some(_) => {}
            }
        } else if header.requests_hash.is_some() {
            return Err("requests hash before Isthmus".into());
        }
        Ok(())
    }

    /// Checks of a header against its parent.
    pub fn validate_against_parent(&self, header: &Header, parent: &Header) -> Result<(), Invalid> {
        let chain = self.chain;
        let parent_hash = parent.hash_slow();
        if header.parent_hash != parent_hash {
            return Err(format!("parent hash {} is not {parent_hash}", header.parent_hash));
        }
        if header.number != parent.number + 1 {
            return Err(format!("number {} does not follow {}", header.number, parent.number));
        }

        let allows_same_timestamp = chain.is_active(BaseFeature::SubsecondBlocks, header.timestamp)
            && chain.is_active(BaseFeature::SubsecondBlocks, parent.timestamp);
        if header.timestamp < parent.timestamp
            || (header.timestamp == parent.timestamp && !allows_same_timestamp)
        {
            return Err(format!(
                "timestamp {} is not after parent {}",
                header.timestamp, parent.timestamp
            ));
        }

        let expected = self.next_base_fee(parent, header.timestamp);
        let base_fee = header.base_fee_per_gas.ok_or("base fee missing")?;
        if expected != Some(base_fee) {
            return Err(format!("base fee {base_fee} is not {expected:?}"));
        }

        if chain.is_active(BaseFeature::BlobHeader, header.timestamp) {
            let blob_gas_used = header.blob_gas_used.ok_or("blob gas used missing")?;
            if !chain.is_active(BaseFeature::DaFootprint, header.timestamp) && blob_gas_used != 0 {
                return Err(format!("blob gas used {blob_gas_used} is not zero"));
            }
            let excess_blob_gas = header.excess_blob_gas.ok_or("excess blob gas missing")?;
            if excess_blob_gas != 0 {
                return Err(format!("excess blob gas {excess_blob_gas} is not zero"));
            }
        }
        Ok(())
    }

    /// Checks of a block body against its header, before execution.
    pub fn validate_pre_execution(
        &self,
        header: &Header,
        transactions: &[BaseTxEnvelope],
        transactions_root: B256,
        withdrawals: Option<&[alloy_eips::eip4895::Withdrawal]>,
    ) -> Result<(), Invalid> {
        let chain = self.chain;
        if transactions_root != header.transactions_root {
            return Err(format!(
                "transactions root {transactions_root} is not {}",
                header.transactions_root
            ));
        }
        if let Some(schedule) = chain.denim_schedule()
            && header.number != schedule.genesis_block_number
        {
            BaseTimeUpdateTx::validate_timestamp_schedule(
                &schedule,
                transactions,
                header.number,
                header.timestamp,
            )
            .map_err(|error| format!("base time: {error}"))?;
        }
        if !chain.is_active(BaseFeature::Withdrawals, header.timestamp) {
            return Ok(());
        }
        if withdrawals.is_some_and(|withdrawals| !withdrawals.is_empty()) {
            return Err("withdrawals are not empty".into());
        }
        if chain.is_active(BaseFeature::DaFootprint, header.timestamp) {
            header.blob_gas_used.ok_or("blob gas used missing")?;
        } else if chain.is_active(BaseFeature::BlobHeader, header.timestamp) {
            let blob_gas_used = header.blob_gas_used.ok_or("blob gas used missing")?;
            // Base has no blob transactions, so the computed blob gas is zero.
            if blob_gas_used != 0 {
                return Err(format!("blob gas used {blob_gas_used} is not zero"));
            }
        }
        if chain.is_active(BaseFeature::L2ToL1MessageRoot, header.timestamp) {
            header.withdrawals_root.ok_or("withdrawals root missing")?;
        } else if header.withdrawals_root != Some(EMPTY_ROOT_HASH) {
            return Err(format!("withdrawals root {:?} is not empty", header.withdrawals_root));
        }
        Ok(())
    }

    /// Checks of execution results against the header.
    pub fn validate_post_execution(
        &self,
        header: &Header,
        computed: &ComputedHeader,
    ) -> Result<(), Invalid> {
        let chain = self.chain;
        if chain.is_active(BaseFeature::DaFootprint, header.timestamp) {
            let expected = header.blob_gas_used.ok_or("blob gas used missing")?;
            if computed.blob_gas_used != Some(expected) {
                return Err(format!(
                    "blob gas used {:?} is not {expected}",
                    computed.blob_gas_used
                ));
            }
        }
        if computed.receipts_root != header.receipts_root {
            return Err(format!(
                "receipts root {} is not {}",
                computed.receipts_root, header.receipts_root
            ));
        }
        if computed.logs_bloom != header.logs_bloom {
            return Err("logs bloom differs".into());
        }
        if computed.gas_used != header.gas_used {
            return Err(format!("gas used {} is not {}", computed.gas_used, header.gas_used));
        }
        if chain.is_active(BaseFeature::L2ToL1MessageRoot, header.timestamp)
            && computed.withdrawals_root != header.withdrawals_root
        {
            return Err(format!(
                "withdrawals root {:?} is not {:?}",
                computed.withdrawals_root, header.withdrawals_root
            ));
        }
        if computed.state_root != header.state_root {
            return Err(format!(
                "state root {} is not {}",
                computed.state_root, header.state_root
            ));
        }
        Ok(())
    }
}

/// The header fields a block's execution determines.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ComputedHeader {
    /// State root after the block.
    pub state_root: B256,
    /// Receipts root.
    pub receipts_root: B256,
    /// Transactions root.
    pub transactions_root: B256,
    /// Bloom of every log.
    pub logs_bloom: Bloom,
    /// Gas used.
    pub gas_used: u64,
    /// Blob gas used (zero from Ecotone, the DA footprint from Jovian).
    pub blob_gas_used: Option<u64>,
    /// Withdrawals root (empty from Canyon, the message passer storage root from Isthmus).
    pub withdrawals_root: Option<B256>,
    /// Requests hash (from Isthmus).
    pub requests_hash: Option<B256>,
}
