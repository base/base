//! Block headers, L1 attributes, and the synthetic system configuration of a chain.

use alloy_consensus::{EMPTY_OMMER_ROOT_HASH, Header};
use alloy_eips::{calc_next_block_base_fee, eip1559::BaseFeeParams, eip7685::EMPTY_REQUESTS_HASH};
use alloy_primitives::{Address, B64, B256, Bloom, Bytes, U256, keccak256};
use base_common_chains::Mainnet;
use base_common_consensus::{
    BaseTimeUpdateTx, HoloceneExtraData, JovianExtraData, L1BlockInfoTx, Predeploys,
    UserDepositSource,
};
use base_common_genesis::{BaseFeature, FeeConfig, RollupConfig, SystemConfig};
use eyre::ensure;

use crate::{
    HeaderResults, RequestHeader,
    r#gen::{era::Schedule, rng::Rng, tx::Txs},
};

/// The synthetic L1 system configuration of a chain, which the L1 attributes deposit, the gas
/// limit, and the extra data carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SysConfig {
    /// System config as the derivation pipeline sees it.
    pub config: SystemConfig,
}

impl SysConfig {
    /// A random plausible configuration, with extremes now and then.
    pub fn random(rng: &mut Rng, timestamp: u64) -> Self {
        let schedule = Schedule::mainnet();
        let mut gas_limit =
            *rng.pick(&[30_000_000u64, 60_000_000, 120_000_000, 240_000_000, 400_000_000]);
        if rng.chance(8) {
            gas_limit = rng.range(5_000_000, 30_000_000);
        }
        let mut denominator = *rng.pick(&[250u32, 50, 8, 1, 1000]);
        if schedule.active(BaseFeature::SubsecondBlocks, timestamp) {
            gas_limit /= u64::from(RollupConfig::DENIM_GAS_PARAMETER_SCALING_FACTOR);
            gas_limit = gas_limit.max(5_000_000);
            denominator =
                denominator.saturating_mul(RollupConfig::DENIM_GAS_PARAMETER_SCALING_FACTOR);
        }
        // Most chains use realistic fee parameters; some use extremes.
        let extreme = rng.chance(15);
        let ecotone_scalar = |rng: &mut Rng| -> U256 {
            // Version 1 scalar: blob base fee scalar in bytes 24..28, base fee scalar in 28..32.
            let blob: u32 = if extreme {
                *rng.pick(&[0u32, 1, 1_000_000, u32::MAX])
            } else {
                *rng.pick(&[810_949u32, 659_851, 1_055_762])
            };
            let base: u32 = if extreme {
                *rng.pick(&[0u32, 1, u32::MAX])
            } else {
                *rng.pick(&[1368u32, 2269, 5000])
            };
            let mut word = [0u8; 32];
            word[0] = 1;
            word[24..28].copy_from_slice(&blob.to_be_bytes());
            word[28..32].copy_from_slice(&base.to_be_bytes());
            U256::from_be_bytes(word)
        };
        let ecotone = schedule.active(BaseFeature::BlobAwareL1Fees, timestamp);
        let scalar = if ecotone && rng.chance(90) {
            ecotone_scalar(rng)
        } else {
            U256::from(*rng.pick(&[0u64, 684_000, 1_000_000]))
        };
        let pick_extreme = |rng: &mut Rng, normal: &[u64], edges: &[u64]| -> u64 {
            if extreme { *rng.pick(edges) } else { *rng.pick(normal) }
        };
        let config = SystemConfig {
            batcher_address: Address::repeat_byte(0x50),
            overhead: U256::from(*rng.pick(&[0u64, 188, 2100])),
            scalar,
            gas_limit,
            base_fee_scalar: None,
            blob_base_fee_scalar: None,
            eip1559_denominator: Some(denominator),
            eip1559_elasticity: Some(*rng.pick(&[6u32, 2, 1, 10, 5])),
            operator_fee_scalar: Some(pick_extreme(
                rng,
                &[0, 0, 1, 1_000],
                &[1_000_000, u64::from(u32::MAX)],
            ) as u32),
            operator_fee_constant: Some(pick_extreme(
                rng,
                &[0, 0, 1, 1_000_000_000],
                &[u64::MAX, 1 << 62],
            )),
            min_base_fee: Some(pick_extreme(
                rng,
                &[0, 1, 1_000_000, 5_000_000],
                &[0, 100_000_000, 1 << 40],
            )),
            da_footprint_gas_scalar: Some(pick_extreme(
                rng,
                &[0, 400, 400],
                &[1, 1, 4_000, u64::from(u16::MAX)],
            ) as u16),
        };
        Self { config }
    }

    /// The header extra data for a block at `timestamp` with parent at `parent_timestamp`.
    pub fn extra_data(&self, parent_timestamp: u64, timestamp: u64) -> Bytes {
        let schedule = Schedule::mainnet();
        let defaults = FeeConfig::BASE_MAINNET.post_canyon_params();
        let Some(params) =
            self.config.eip_1559_params(&schedule.rollup, parent_timestamp, timestamp)
        else {
            return Bytes::new();
        };
        if schedule.active(BaseFeature::MinBaseFee, timestamp) {
            JovianExtraData::encode(params, defaults, self.config.min_base_fee.unwrap_or_default())
                .unwrap_or_default()
        } else {
            HoloceneExtraData::encode(params, defaults).unwrap_or_default()
        }
    }
}

/// The L1 origin a chain currently derives from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct L1Origin {
    /// L1 header.
    pub header: Header,
    /// Sequence number of the next L2 block within this epoch.
    pub sequence_number: u64,
}

impl L1Origin {
    /// A random L1 header no later than `l2_timestamp`.
    pub fn random(rng: &mut Rng, l2_timestamp: u64) -> Self {
        let excess_blob_gas = match rng.below(6) {
            0 => 0,
            1 => rng.range(1, 10_000_000),
            2 => rng.range(10_000_000, 500_000_000),
            _ => rng.range(0, 3_000_000),
        };
        let header = Header {
            parent_hash: rng.b256(),
            number: 17_000_000 + (l2_timestamp - 1_686_789_347) / 12,
            timestamp: l2_timestamp.saturating_sub(rng.range(0, 60)),
            base_fee_per_gas: Some(match rng.below(30) {
                0 => u64::MAX >> 8,
                1 => 7,
                2..=10 => 1_000_000_000,
                _ => rng.range(1, 100_000_000_000),
            }),
            excess_blob_gas: Some(excess_blob_gas),
            blob_gas_used: Some(0),
            mix_hash: rng.b256(),
            parent_beacon_block_root: Some(rng.b256()),
            gas_limit: 36_000_000,
            ..Default::default()
        };
        Self { header, sequence_number: 0 }
    }

    /// Advances to the next L2 block: same epoch, or (with probability `new_epoch`%) a new one.
    pub fn advance(&mut self, rng: &mut Rng, l2_timestamp: u64, new_epoch: u64) {
        if rng.chance(new_epoch) && self.header.timestamp + 12 <= l2_timestamp {
            let mut next = Self::random(rng, l2_timestamp);
            next.header.number = self.header.number + 1;
            next.header.parent_hash = self.header.hash_slow();
            next.header.timestamp = self.header.timestamp + 12;
            *self = next;
        } else {
            self.sequence_number += 1;
        }
    }

    /// The source hash for the `index`-th user deposit of this epoch.
    pub fn deposit_source(&self, index: u64) -> B256 {
        UserDepositSource::new(self.header.hash_slow(), index).source_hash()
    }
}

/// Header construction helpers.
#[derive(Debug)]
pub struct Blocks;

impl Blocks {
    /// Upper bound on generated base fees, far from where base fee arithmetic overflows.
    pub const MAX_BASE_FEE: u64 = 1_000_000_000_000_000;

    /// The L1 attributes deposit for a block.
    pub fn l1_info_deposit(
        sys: &SysConfig,
        origin: &L1Origin,
        parent_timestamp: u64,
        timestamp: u64,
    ) -> Bytes {
        let schedule = Schedule::mainnet();
        let (_, deposit) = L1BlockInfoTx::try_new_with_deposit_tx(
            &schedule.rollup,
            &Mainnet::l1_config(),
            &sys.config,
            origin.sequence_number,
            &origin.header,
            parent_timestamp,
            timestamp,
        )
        .expect("L1 info for a valid config");
        Txs::deposit(deposit.into_inner())
    }

    /// The `BaseTime` deposit for a Denim block.
    pub fn base_time_deposit(number: u64, millis: u16) -> Bytes {
        let update = BaseTimeUpdateTx::new(millis).expect("schedule millis are 200 ms aligned");
        Txs::deposit(update.into_deposit_tx(number).into_inner())
    }

    /// The base fee a block at `timestamp` derives from `parent`, following Base's rules.
    pub fn next_base_fee(parent: &Header, timestamp: u64) -> u64 {
        let schedule = Schedule::mainnet();
        let base_fee = parent.base_fee_per_gas.unwrap_or_default();
        let fixed = |timestamp: u64| {
            if schedule.active(BaseFeature::BaseFeeDenominator, timestamp) {
                FeeConfig::BASE_MAINNET.post_canyon_params()
            } else {
                FeeConfig::BASE_MAINNET.pre_canyon_params()
            }
        };
        let params = |elasticity: u32, denominator: u32| {
            if elasticity == 0 && denominator == 0 {
                Some(fixed(timestamp))
            } else if elasticity == 0 || denominator == 0 {
                None
            } else {
                Some(BaseFeeParams::new(u128::from(denominator), u128::from(elasticity)))
            }
        };
        if schedule.active(BaseFeature::DaAwareBaseFee, parent.timestamp) {
            let Ok((elasticity, denominator, min)) = JovianExtraData::decode(&parent.extra_data)
            else {
                return 0;
            };
            let Some(params) = params(elasticity, denominator) else { return 0 };
            let gas_used = parent.gas_used.max(parent.blob_gas_used.unwrap_or_default());
            return calc_next_block_base_fee(gas_used, parent.gas_limit, base_fee, params).max(min);
        }
        if schedule.active(BaseFeature::DynamicBaseFeeParameters, parent.timestamp) {
            let Ok((elasticity, denominator)) = HoloceneExtraData::decode(&parent.extra_data)
            else {
                return 0;
            };
            let Some(params) = params(elasticity, denominator) else { return 0 };
            return calc_next_block_base_fee(parent.gas_used, parent.gas_limit, base_fee, params);
        }
        calc_next_block_base_fee(parent.gas_used, parent.gas_limit, base_fee, fixed(timestamp))
    }

    /// The request header inputs for a block.
    #[allow(clippy::too_many_arguments)]
    pub fn request_header(
        number: u64,
        timestamp: u64,
        coinbase: Address,
        origin: &L1Origin,
        gas_limit: u64,
        base_fee: u64,
        extra_data: Bytes,
    ) -> RequestHeader {
        let schedule = Schedule::mainnet();
        let cancun = schedule.active(BaseFeature::BeaconRoots, timestamp);
        RequestHeader {
            number,
            timestamp,
            coinbase,
            prev_randao: origin.header.mix_hash,
            gas_limit,
            base_fee_per_gas: base_fee,
            extra_data,
            parent_beacon_block_root: cancun
                .then(|| origin.header.parent_beacon_block_root.unwrap_or_default()),
            excess_blob_gas: cancun.then_some(0),
            results: None,
        }
    }

    /// Assembles the full header of a built block from its inputs and the computed fields, and
    /// checks it hashes to the computed hash.
    pub fn assemble(
        parent_hash: B256,
        input: &RequestHeader,
        results: &HeaderResults,
    ) -> eyre::Result<Header> {
        let header = Header {
            parent_hash,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            beneficiary: input.coinbase,
            state_root: results.state_root,
            transactions_root: results.transactions_root,
            receipts_root: results.receipts_root,
            logs_bloom: results.logs_bloom,
            difficulty: U256::ZERO,
            number: input.number,
            gas_limit: input.gas_limit,
            gas_used: results.gas_used,
            timestamp: input.timestamp,
            extra_data: input.extra_data.clone(),
            mix_hash: input.prev_randao,
            nonce: B64::ZERO,
            base_fee_per_gas: Some(input.base_fee_per_gas),
            withdrawals_root: results.withdrawals_root,
            blob_gas_used: results.blob_gas_used,
            excess_blob_gas: input.excess_blob_gas,
            parent_beacon_block_root: input.parent_beacon_block_root,
            requests_hash: results.requests_hash,
            block_access_list_hash: None,
            slot_number: None,
        };
        ensure!(
            header.hash_slow() == results.hash,
            "assembled header hash {} does not match computed hash {}",
            header.hash_slow(),
            results.hash
        );
        Ok(header)
    }

    /// A synthetic ancestor chain of `count` headers ending at block `parent_number` whose state
    /// root is `state_root` and (from Isthmus) withdrawals root is `message_passer_root`.
    /// Returned newest first.
    pub fn synthetic_ancestors(
        rng: &mut Rng,
        sys: &SysConfig,
        parent_number: u64,
        count: u64,
        state_root: B256,
        message_passer_root: B256,
    ) -> Vec<Header> {
        let schedule = Schedule::mainnet();
        let first = parent_number + 1 - count.min(parent_number);
        let mut headers: Vec<Header> = Vec::new();
        let mut parent_hash = rng.b256();
        let mut base_fee = *rng.pick(&[1_000_000u64, 5_000_000, 50_000_000, 1_000_000_000, 7]);
        for number in first..=parent_number {
            let timestamp = schedule.timestamp_parts(number).0;
            let parent_timestamp = schedule.timestamp_parts(number.saturating_sub(1)).0;
            let is_parent = number == parent_number;
            let gas_limit = sys.config.gas_limit;
            let gas_used = match rng.below(6) {
                0 => 0,
                1 => gas_limit,
                2 => gas_limit / 2,
                3 => gas_limit / u64::from(sys.config.eip1559_elasticity.unwrap_or(6)).max(1),
                _ => rng.below(gas_limit),
            };
            let cancun = schedule.active(BaseFeature::BeaconRoots, timestamp);
            let jovian = schedule.active(BaseFeature::DaFootprint, timestamp);
            let isthmus = schedule.active(BaseFeature::L2ToL1MessageRoot, timestamp);
            let canyon = schedule.active(BaseFeature::Withdrawals, timestamp);
            let mut header = Header {
                parent_hash,
                ommers_hash: EMPTY_OMMER_ROOT_HASH,
                beneficiary: Predeploys::SEQUENCER_FEE_VAULT,
                state_root: if is_parent { state_root } else { rng.b256() },
                transactions_root: rng.b256(),
                receipts_root: rng.b256(),
                logs_bloom: Bloom::default(),
                difficulty: U256::ZERO,
                number,
                gas_limit,
                gas_used,
                timestamp,
                extra_data: sys.extra_data(parent_timestamp, timestamp),
                mix_hash: rng.b256(),
                nonce: B64::ZERO,
                base_fee_per_gas: Some(base_fee),
                withdrawals_root: canyon.then(|| {
                    if isthmus {
                        if is_parent { message_passer_root } else { rng.b256() }
                    } else {
                        alloy_trie::EMPTY_ROOT_HASH
                    }
                }),
                blob_gas_used: cancun.then(|| if jovian { rng.below(gas_limit) } else { 0 }),
                excess_blob_gas: cancun.then_some(0),
                parent_beacon_block_root: cancun.then(|| rng.b256()),
                requests_hash: isthmus.then_some(EMPTY_REQUESTS_HASH),
                block_access_list_hash: None,
                slot_number: None,
            };
            if !canyon {
                header.withdrawals_root = None;
            }
            parent_hash = header.hash_slow();
            let next_timestamp = schedule.timestamp_parts(number + 1).0;
            base_fee = Self::next_base_fee(&header, next_timestamp).clamp(1, Self::MAX_BASE_FEE);
            headers.push(header);
        }
        headers.reverse();
        headers
    }

    /// The hash of an RLP header.
    pub fn hash(header: &Header) -> B256 {
        keccak256(alloy_rlp::encode(header))
    }
}
