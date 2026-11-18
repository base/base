//! Assembly of one build-mode request from a known pre-state.

use alloy_consensus::Header;
use alloy_eips::Decodable2718;
use alloy_primitives::Bytes;
use base_common_consensus::{BaseTxEnvelope, Predeploys};
use base_common_genesis::BaseFeature;
use base_consensus_upgrades::{Upgrade, Upgrades};

use crate::{
    Mode, Request,
    r#gen::{
        block::{Blocks, L1Origin, SysConfig},
        era::Schedule,
        keys::Keys,
        plan::{BlockCtx, PlannedTx, Planner},
        registry::Registry,
        rng::Rng,
        state::WorldState,
    },
};

/// The chain name every request carries.
pub const CHAIN: &str = "base-mainnet";

/// How many transactions to plan and how many of them to make invalid.
#[derive(Debug, Clone, Copy)]
pub struct TrafficMix {
    /// Minimum random transactions per block.
    pub min_txs: usize,
    /// Maximum random transactions per block.
    pub max_txs: usize,
    /// Percentage of random transactions that are deliberately invalid.
    pub invalid_percent: u64,
    /// See `GeneratorOptions::max_nonce_deposit_creates`.
    pub max_nonce_deposit_creates: bool,
}

/// A planned block: the request and what each transaction was meant to do.
#[derive(Debug, Clone)]
pub struct Template {
    /// The build-mode request.
    pub request: Request,
    /// One label per transaction, in block order.
    pub labels: Vec<String>,
    /// Planned non-system transactions with their index in the block.
    pub planned: Vec<(usize, PlannedTx)>,
    /// Runtimes the block may deploy (to register before applying its post-state).
    pub new_codes: Vec<Bytes>,
}

/// Inputs to [`Template::build`].
#[derive(Debug)]
pub struct TemplateInput<'a> {
    /// Case id.
    pub id: String,
    /// Pre-state.
    pub state: &'a WorldState,
    /// Chain knowledge.
    pub registry: &'a Registry,
    /// System configuration.
    pub sys: &'a SysConfig,
    /// L1 origin.
    pub origin: &'a L1Origin,
    /// Ancestors, newest first.
    pub ancestors: &'a [Header],
    /// Traffic mix.
    pub mix: TrafficMix,
    /// Mainnet block number minus chain block number: a chain whose genesis was moved to block
    /// zero still takes its timestamps from mainnet's schedule at the original numbers.
    pub number_offset: u64,
    /// Seconds added to the block's schedule timestamp (chains moved off the mainnet schedule).
    pub time_offset: u64,
}

impl Template {
    /// Plans a block on top of `input.ancestors[0]`. `scenario` adds deposits (first call, with
    /// `true`) and sequencer transactions (second call, with `false`) before random traffic.
    pub fn build(
        rng: &mut Rng,
        input: TemplateInput<'_>,
        scenario: &mut dyn FnMut(&mut Planner<'_>, bool),
    ) -> Self {
        let schedule = Schedule::mainnet();
        let parent = &input.ancestors[0];
        let number = parent.number + 1;
        let (timestamp, millis) = schedule.timestamp_parts(number + input.number_offset);
        let timestamp = timestamp + input.time_offset;
        let upgrade = schedule.upgrade_at(timestamp);

        let mut base_fee = Blocks::next_base_fee(parent, timestamp).min(Blocks::MAX_BASE_FEE);
        if rng.chance(4) {
            base_fee = *rng.pick(&[0u64, 1, base_fee / 2, base_fee.saturating_mul(3)]);
        }
        let coinbase = if rng.chance(97) {
            Predeploys::SEQUENCER_FEE_VAULT
        } else {
            Keys::get().address(rng.index(Keys::COUNT))
        };
        let extra_data = input.sys.extra_data(parent.timestamp, timestamp);
        let gas_limit = input.sys.config.gas_limit;
        let header = Blocks::request_header(
            number,
            timestamp,
            coinbase,
            input.origin,
            gas_limit,
            base_fee,
            extra_data,
        );

        let mut transactions =
            vec![Blocks::l1_info_deposit(input.sys, input.origin, parent.timestamp, timestamp)];
        let mut labels = vec!["l1info".to_owned()];
        let mut system_gas = 1_000_000;
        if schedule.active(BaseFeature::SubsecondBlocks, timestamp) {
            transactions.push(Blocks::base_time_deposit(number, millis));
            labels.push("baseTime".to_owned());
            system_gas += 1_000_000;
        }
        let mut upgrade_txs: Vec<Bytes> = Vec::new();
        let activation = |feature| {
            schedule.rollup.is_feature_activation_block_cascading(
                feature,
                timestamp,
                parent.timestamp,
            )
        };
        if activation(BaseFeature::EcotoneUpgradeTransactions) {
            upgrade_txs.extend(Upgrades::ECOTONE.txs());
        }
        if activation(BaseFeature::FjordUpgradeTransactions) {
            upgrade_txs.extend(Upgrades::FJORD.txs());
        }
        if activation(BaseFeature::IsthmusUpgradeTransactions) {
            upgrade_txs.extend(Upgrades::ISTHMUS.txs());
        }
        if activation(BaseFeature::JovianUpgradeTransactions) {
            upgrade_txs.extend(Upgrades::JOVIAN.txs());
        }
        for raw in &upgrade_txs {
            if let Ok(BaseTxEnvelope::Deposit(deposit)) =
                BaseTxEnvelope::decode_2718(&mut raw.as_ref())
            {
                system_gas += deposit.inner().gas_limit;
            }
        }

        let ctx = BlockCtx {
            number,
            timestamp,
            upgrade,
            gas_limit,
            base_fee,
            origin: input.origin.clone(),
            deposits_allowed: input.origin.sequence_number == 0,
        };
        let mut planner = Planner::new(rng.fork(1), ctx, input.state, input.registry, system_gas);
        planner.max_nonce_deposit_creates = input.mix.max_nonce_deposit_creates;
        if planner.ctx.deposits_allowed {
            scenario(&mut planner, true);
            for _ in 0..planner.rng.below(4) {
                planner.deposit();
            }
        }
        let deposit_count = planner.out.len();
        scenario(&mut planner, false);
        let count = planner.rng.range(input.mix.min_txs as u64, input.mix.max_txs as u64);
        for _ in 0..count {
            planner.random(input.mix.invalid_percent);
        }

        // User deposits (including deposits that act for keyless privileged accounts) come
        // first, then the upgrade transactions, then sequencer transactions.
        let mut planned = planner.out;
        let is_deposit = |tx: &PlannedTx| tx.raw.first() == Some(&0x7e);
        let (deposits, sequenced): (Vec<_>, Vec<_>) = {
            let tail = planned.split_off(deposit_count);
            let mut deposits = planned;
            let mut sequenced = Vec::new();
            for tx in tail {
                if is_deposit(&tx) { deposits.push(tx) } else { sequenced.push(tx) }
            }
            (deposits, sequenced)
        };
        let mut ordered = Vec::new();
        for tx in deposits {
            ordered.push((transactions.len(), tx.clone()));
            transactions.push(tx.raw);
            labels.push(tx.label);
        }
        for raw in upgrade_txs {
            transactions.push(raw);
            labels.push("upgrade".to_owned());
        }
        for tx in sequenced {
            ordered.push((transactions.len(), tx.clone()));
            transactions.push(tx.raw);
            labels.push(tx.label);
        }

        let (_, witness) = input.state.witness();
        let request = Request {
            id: input.id,
            chain: CHAIN.to_owned(),
            mode: Mode::Build,
            ancestors: input.ancestors.iter().map(|h| Bytes::from(alloy_rlp::encode(h))).collect(),
            header,
            transactions,
            witness,
            trace: None,
            system_config: None,
        };
        Self { request, labels, planned: ordered, new_codes: planner.new_codes }
    }
}
