//! Multi-block synthetic chains driven by the agreed responses.

use alloy_consensus::Header;
use alloy_primitives::Address;
use base_common_genesis::{BaseFeature, RollupConfig};
use eyre::{bail, ensure};
use serde::{Deserialize, Serialize};

use crate::{
    Request, Response,
    r#gen::{
        block::{Blocks, L1Origin, SysConfig},
        case::{GeneratorOptions, era_blocks},
        era::{Era, Schedule},
        genesis::Genesis,
        registry::Registry,
        rng::Rng,
        scenario::Scenarios,
        state::WorldState,
        stress::{Stress, StressPlan},
        sysconfig::ConfigLogs,
        template::{Template, TemplateInput, TrafficMix},
    },
};

/// Forks a chain may be started just before, so that it crosses the activation. Everest counts
/// only when it has an activation time (it is then among the targeted eras).
const CROSSINGS: [Era; 7] =
    [Era::Isthmus, Era::Jovian, Era::Azul, Era::Beryl, Era::Cobalt, Era::Denim, Era::Everest];

/// A change to a chain's pending block, for coverage-guided chain fuzzing. The chain after an
/// edit is still a deterministic function of the seed, the edits, and the agreed responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum ChainEdit {
    /// Re-plans the block from a new random stream, which the rest of the chain then follows.
    Reroll {
        /// Seeds the new stream.
        salt: u64,
    },
    /// Starts a new epoch at the block with a system config update, when the L1 origin can
    /// advance (or already starts an epoch there).
    ConfigUpdate {
        /// Seeds the update.
        salt: u64,
    },
    /// Moves the block, and every later block, this many seconds later than the schedule.
    Gap {
        /// Seconds added.
        secs: u64,
    },
}

/// Generates a chain of build-mode requests. Each request after the first builds on the
/// previous block as the clients computed it: the parent header is assembled from the previous
/// request's inputs and the agreed response's computed fields, and the pre-state is the
/// previous pre-state with the agreed post-state diff applied.
///
/// The chain is a deterministic function of the seed, the options, the agreed responses, the
/// [`Self::prepare`] calls, and the [`Self::edit`] calls.
#[derive(Debug, Clone)]
pub struct ChainGenerator {
    seed: u64,
    options: GeneratorOptions,
    rng: Rng,
    state: WorldState,
    registry: Registry,
    sys: SysConfig,
    origin: L1Origin,
    ancestors: Vec<Header>,
    scenarios: Scenarios,
    pending: Template,
    blocks: u64,
    /// Further consecutive blocks that start an epoch with a system config update.
    burst: u64,
    /// State-stress traffic of the pending block.
    stress: Option<StressPlan>,
    /// Mainnet block number minus chain block number (see [`Self::rebase`]).
    number_offset: u64,
    /// Seconds every block's timestamp is past the mainnet schedule (see [`ChainEdit::Gap`]).
    time_offset: u64,
}

impl ChainGenerator {
    /// Most ancestors a request carries.
    pub const MAX_ANCESTORS: usize = 256;

    /// Starts a chain: builds the synthetic genesis and plans the first block.
    pub fn new(seed: u64, options: GeneratorOptions) -> Self {
        assert!(!options.eras.is_empty(), "at least one era");
        let schedule = Schedule::mainnet();
        let mut rng = Rng::new(&[seed, u64::MAX]);
        let crossings: Vec<Era> =
            CROSSINGS.into_iter().filter(|era| options.eras.contains(era)).collect();
        let parent_number = if !crossings.is_empty()
            && rng.chance((options.activation_percent.max(1) * 3).min(100))
        {
            let fork = *rng.pick(&crossings);
            schedule.first_block_at(fork.activation_timestamp()) - rng.range(1, 6)
        } else {
            let era = options.pick_era(&mut rng);
            let (first, last) = era_blocks(era);
            rng.range(first.max(2), last.saturating_sub(10_000).max(first + 1)) - 1
        };
        let parent_timestamp = schedule.timestamp_parts(parent_number).0;
        let era = Era::at(parent_timestamp);
        let sys = SysConfig::random(&mut rng, schedule.timestamp_parts(parent_number + 1).0);
        // BLOCKHASH can reach 256 blocks back, so requests carry the full window.
        let genesis =
            Genesis::build(&mut rng, era, parent_number, &sys, Self::MAX_ANCESTORS as u64);
        let origin = L1Origin::random(&mut rng, parent_timestamp);
        let scenarios = Scenarios::new(&mut rng, era);
        let mut state = genesis.state;
        let mut ancestors = genesis.ancestors;
        if options.state_stress {
            Stress::seed_genesis(&mut state, seed);
            ancestors[0].state_root = state.witness().0;
        }
        let mut chain = Self {
            seed,
            options,
            rng,
            state,
            registry: genesis.registry,
            sys,
            origin,
            ancestors,
            scenarios,
            pending: Self::placeholder(),
            blocks: 0,
            burst: 0,
            stress: None,
            number_offset: 0,
            time_offset: 0,
        };
        chain.pending = chain.plan();
        chain
    }

    fn placeholder() -> Template {
        Template {
            request: Request {
                id: String::new(),
                chain: String::new(),
                mode: crate::Mode::Build,
                ancestors: Vec::new(),
                header: Default::default(),
                transactions: Vec::new(),
                witness: Default::default(),
                trace: None,
                system_config: None,
            },
            labels: Vec::new(),
            planned: Vec::new(),
            new_codes: Vec::new(),
        }
    }

    /// The request awaiting a response.
    pub const fn request(&self) -> &Request {
        &self.pending.request
    }

    /// One label per transaction of the pending request.
    pub fn labels(&self) -> &[String] {
        &self.pending.labels
    }

    /// Number of blocks applied so far.
    pub const fn blocks(&self) -> u64 {
        self.blocks
    }

    /// The current state (the pending request's pre-state).
    pub const fn state(&self) -> &WorldState {
        &self.state
    }

    /// What the generator knows about the chain.
    pub const fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Ancestor headers, newest (the pending request's parent) first.
    pub fn ancestors(&self) -> &[Header] {
        &self.ancestors
    }

    /// Replaces the ancestors with the single header `parent` and re-plans the pending block on
    /// it. Real nodes started from a genesis file at the parent block compute their own genesis
    /// header (empty body, genesis defaults) and know no earlier blocks; rebasing makes the chain
    /// build on that header. `parent` must commit to the current state. A `parent` at a lower
    /// number (a genesis file moved to block zero) shifts every later block number down by the
    /// difference, while timestamps still follow mainnet's schedule at the original numbers.
    pub fn rebase(&mut self, parent: Header) -> eyre::Result<()> {
        ensure!(self.blocks == 0, "only an unstarted chain can be rebased");
        self.state.check_root(parent.state_root)?;
        self.number_offset = self.ancestors[0]
            .number
            .checked_sub(parent.number)
            .ok_or_else(|| eyre::eyre!("cannot rebase onto a later block number"))?;
        self.ancestors = vec![parent];
        self.pending = self.plan();
        Ok(())
    }

    /// Sets the pending request's base fee. Build mode executes with whatever base fee the
    /// request names, and the generator sometimes picks a wrong one on purpose; a block meant for
    /// real nodes must carry the base fee its parent implies.
    pub const fn set_base_fee(&mut self, base_fee: u64) {
        self.pending.request.header.base_fee_per_gas = base_fee;
    }

    /// Re-plans the pending block from fresh randomness drawn from `salt` and the block height,
    /// with `stress` traffic (which needs `GeneratorOptions::state_stress`). Two generators in the
    /// same state prepared with the same arguments plan the same block; different salts give
    /// sibling blocks.
    pub fn prepare(&mut self, stress: Option<StressPlan>, salt: u64) {
        self.stress = stress.filter(|_| self.options.state_stress);
        self.rng = Rng::new(&[self.seed, salt, self.blocks, 0x57a7_e000]);
        self.pending = self.plan();
    }

    fn plan(&mut self) -> Template {
        let number = self.ancestors[0].number + 1;
        let input = TemplateInput {
            id: format!("chain-{:x}-{}-{number}", self.seed, self.blocks),
            state: &self.state,
            registry: &self.registry,
            sys: &self.sys,
            origin: &self.origin,
            ancestors: &self.ancestors,
            number_offset: self.number_offset,
            mix: self.stress.map_or_else(
                || self.options.mix(),
                |stress| {
                    let txs = usize::from(stress.random_txs);
                    TrafficMix { min_txs: txs, max_txs: txs, ..self.options.mix() }
                },
            ),
            time_offset: self.time_offset,
        };
        let scenarios = &mut self.scenarios;
        let stress = self.stress;
        Template::build(&mut self.rng, input, &mut |planner, deposits| {
            scenarios.step(planner, deposits);
            if let Some(stress) = &stress
                && !deposits
            {
                Stress::plan(planner, stress);
            }
        })
    }

    /// Applies the agreed response to the pending request and plans the next block.
    pub fn next(&mut self, agreed: &Response) -> eyre::Result<Request> {
        self.advance(agreed)?;
        self.pending = self.plan();
        Ok(self.pending.request.clone())
    }

    /// Drops the pending block's plan (its request carries a witness of the whole state) until
    /// [`Self::prepare`] plans one again, so idle snapshots of the chain stay small.
    pub fn park(&mut self) {
        self.pending = Self::placeholder();
    }

    /// Applies the agreed response to the pending request without planning the next block:
    /// call [`Self::prepare`] before the next [`Self::request`].
    pub fn advance(&mut self, agreed: &Response) -> eyre::Result<()> {
        let request = &self.pending.request;
        ensure!(agreed.id == request.id, "response {} does not answer {}", agreed.id, request.id);
        if !agreed.ok {
            bail!("agreed response is not ok: {:?}", agreed.error);
        }
        if !agreed.valid {
            bail!("block could not be built: {:?}", agreed.invalid_reason);
        }
        let results =
            agreed.header.as_ref().ok_or_else(|| eyre::eyre!("response has no header"))?;
        let header = Blocks::assemble(self.ancestors[0].hash_slow(), &request.header, results)?;

        for code in std::mem::take(&mut self.pending.new_codes) {
            self.state.register_code(code);
        }
        self.state.apply_post(&agreed.post)?;
        self.state.check_root(header.state_root)?;

        // Included transactions, in order, line up with the receipts.
        let rejected: std::collections::BTreeSet<usize> =
            agreed.rejected.iter().map(|r| r.index).collect();
        let included: Vec<usize> =
            (0..request.transactions.len()).filter(|i| !rejected.contains(i)).collect();
        ensure!(
            included.len() == agreed.receipts.len(),
            "{} included transactions but {} receipts",
            included.len(),
            agreed.receipts.len()
        );
        for receipt in agreed.receipts.iter().filter(|receipt| receipt.status == 1) {
            self.registry.learn_logs(&receipt.logs);
        }
        for (index, planned) in &self.pending.planned {
            if !rejected.contains(index)
                && let Some(deploy) = &planned.deploy
            {
                self.registry.pending.push(deploy.clone());
            }
        }
        let changed: Vec<Address> = agreed.post.keys().copied().collect();
        self.registry.sync_state(&self.state, &changed);

        self.ancestors.insert(0, header);
        self.ancestors.truncate(Self::MAX_ANCESTORS);
        self.blocks += 1;

        let schedule = Schedule::mainnet();
        let parent_timestamp = self.ancestors[0].timestamp;
        let next_timestamp =
            schedule.timestamp_parts(self.ancestors[0].number + 1 + self.number_offset).0
                + self.time_offset;
        // Upgrade activation blocks start a new epoch when they can, and usually change the
        // system config, so config changes land on fork boundaries. A burst gives consecutive
        // blocks an update each, for as long as the L1 origin can advance every block.
        let activation = Era::all().iter().copied().any(|era| {
            (parent_timestamp + 1..=next_timestamp).contains(&era.activation_timestamp())
        });
        let forced = activation || self.burst > 0;
        self.origin.advance(&mut self.rng, next_timestamp, if forced { 100 } else { 35 });
        let update = self.origin.sequence_number == 0
            && (self.burst > 0 || self.rng.chance(if activation { 75 } else { 30 }));
        self.burst = if update { self.burst.saturating_sub(1) } else { 0 };
        if update {
            let mut rng = std::mem::replace(&mut self.rng, Rng::new(&[]));
            self.config_update(&mut rng, parent_timestamp);
            self.rng = rng;
            if self.burst == 0 && self.rng.chance(30) {
                self.burst = self.rng.range(1, 4);
            }
        }
        if Self::denim_activation(next_timestamp, parent_timestamp) {
            self.denim_scaling();
        }
        Ok(())
    }

    /// Applies an edit to the pending block and re-plans it. Returns whether the edit applied:
    /// a config update needs an L1 origin that can start a new epoch at the block.
    pub fn edit(&mut self, edit: ChainEdit) -> bool {
        let parent_timestamp = self.ancestors[0].timestamp;
        let timestamp = self.pending.request.header.timestamp;
        match edit {
            ChainEdit::Reroll { salt } => {
                self.rng = Rng::new(&[self.seed, salt, self.blocks]);
            }
            ChainEdit::ConfigUpdate { salt } => {
                let mut rng = Rng::new(&[self.seed, salt, self.blocks, 1]);
                if self.origin.sequence_number != 0 {
                    let mut origin = self.origin.clone();
                    origin.advance(&mut rng, timestamp, 100);
                    if origin.sequence_number != 0 {
                        return false;
                    }
                    self.origin = origin;
                }
                self.config_update(&mut rng, parent_timestamp);
                self.rng = rng;
            }
            ChainEdit::Gap { secs } => {
                // Offsets only grow, so a block that was an upgrade's activation block still is.
                let crossed = Self::denim_activation(timestamp, parent_timestamp);
                self.time_offset += secs;
                if !crossed && Self::denim_activation(timestamp + secs, parent_timestamp) {
                    self.denim_scaling();
                }
            }
        }
        self.pending = self.plan();
        true
    }

    /// Applies one system config update, as the L1 origin's logs would: derivation applies them
    /// with `update_with_receipts` at the first block of the epoch.
    fn config_update(&mut self, rng: &mut Rng, parent_timestamp: u64) {
        let receipt = ConfigLogs::chain_update(rng, &self.sys.config, parent_timestamp);
        self.sys.config.update_with_receipts(
            &[(&receipt).into()],
            ConfigLogs::address(),
            Schedule::mainnet().active(BaseFeature::BlobAwareL1Fees, self.origin.header.timestamp),
        );
    }

    fn denim_activation(timestamp: u64, parent_timestamp: u64) -> bool {
        Schedule::mainnet().rollup.is_feature_activation_block_cascading(
            BaseFeature::SubsecondBlocks,
            timestamp,
            parent_timestamp,
        )
    }

    /// Denim scales the legacy whole-second gas parameters at its activation block.
    fn denim_scaling(&mut self) {
        let factor = RollupConfig::DENIM_GAS_PARAMETER_SCALING_FACTOR;
        self.sys.config.gas_limit = (self.sys.config.gas_limit / u64::from(factor)).max(5_000_000);
        self.sys.config.eip1559_denominator =
            self.sys.config.eip1559_denominator.map(|d| d.saturating_mul(factor));
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, time::Duration};

    use super::{ChainEdit, ChainGenerator};
    use crate::{
        Adapter, AdapterSpec, Request, Response,
        r#gen::{
            case::{Generator, GeneratorOptions},
            era::{Era, Schedule},
        },
    };

    /// Reach statistics over requests and their responses.
    #[derive(Default)]
    struct Reach {
        requests: u64,
        ok: u64,
        txs: u64,
        included: u64,
        succeeded: u64,
        labels: BTreeMap<String, [u64; 3]>,
        errors: BTreeMap<String, u64>,
        rejections: BTreeMap<String, u64>,
    }

    impl Reach {
        fn record(&mut self, request: &Request, labels: &[String], response: &Response) {
            self.requests += 1;
            if !response.ok || !response.valid {
                let error = response
                    .error
                    .clone()
                    .or_else(|| response.invalid_reason.clone())
                    .unwrap_or_default();
                *self.errors.entry(error.chars().take(160).collect()).or_default() += 1;
                return;
            }
            self.ok += 1;
            let rejected: std::collections::BTreeSet<usize> =
                response.rejected.iter().map(|r| r.index).collect();
            let mut receipts = response.receipts.iter();
            for (index, label) in labels.iter().enumerate().take(request.transactions.len()) {
                let entry = self
                    .labels
                    .entry(label.split(':').take(2).collect::<Vec<_>>().join(":"))
                    .or_default();
                entry[0] += 1;
                self.txs += 1;
                if rejected.contains(&index) {
                    if !label.starts_with("invalid:") {
                        let reason = response
                            .rejected
                            .iter()
                            .find(|r| r.index == index)
                            .map(|r| r.error.as_str())
                            .unwrap_or_default();
                        let reason = reason.split_once("): ").map_or(reason, |(_, rest)| rest);
                        let key = format!(
                            "{} <- {}",
                            reason.chars().take(90).collect::<String>(),
                            label.split(':').take(2).collect::<Vec<_>>().join(":")
                        );
                        *self.rejections.entry(key).or_default() += 1;
                    }
                    continue;
                }
                let receipt = receipts.next().expect("one receipt per included transaction");
                entry[1] += 1;
                self.included += 1;
                if receipt.status == 1 {
                    entry[2] += 1;
                    self.succeeded += 1;
                }
            }
        }

        fn report(&self, title: &str) {
            let pct = |a: u64, b: u64| if b == 0 { 0.0 } else { 100.0 * a as f64 / b as f64 };
            println!("== {title}");
            println!(
                "requests {} ok {:.1}% | txs {} included {:.1}% | included succeeded {:.1}%",
                self.requests,
                pct(self.ok, self.requests),
                self.txs,
                pct(self.included, self.txs),
                pct(self.succeeded, self.included)
            );
            for (label, [total, included, succeeded]) in &self.labels {
                println!(
                    "  {label:48} total {total:6} included {included:6} succeeded {succeeded:6}"
                );
            }
            for (error, count) in &self.errors {
                println!("  error x{count}: {error}");
            }
            for (reason, count) in &self.rejections {
                println!("  rejected x{count}: {reason}");
            }
        }
    }

    /// Drives single-block cases and chains through the adapter named by `DIFF_ADAPTER` and
    /// prints reach statistics. Run with
    /// `DIFF_ADAPTER=target/debug/base-diff-adapter cargo test -p base-diff-fuzz reach -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs an adapter binary"]
    fn reach_against_adapter() {
        let path = std::env::var("DIFF_ADAPTER").expect("DIFF_ADAPTER");
        let env = |name: &str, default: u64| {
            std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
        };
        let (cases, chains, length, seed) = (
            env("DIFF_CASES", 200),
            env("DIFF_CHAINS", 10),
            env("DIFF_CHAIN_LEN", 30),
            env("DIFF_SEED", 1),
        );
        let mut adapter = Adapter::new(
            AdapterSpec::parse(&format!("base={path}")).unwrap(),
            Duration::from_secs(120),
        );

        let generator = Generator::new(seed, GeneratorOptions::default());
        let mut reach = Reach::default();
        for index in 0..cases {
            let template = generator.template(index);
            match adapter.call(&template.request) {
                Ok(response) => reach.record(&template.request, &template.labels, &response),
                Err(failure) => *reach.errors.entry(format!("call: {failure}")).or_default() += 1,
            }
        }
        reach.report("single blocks");

        let mut reach = Reach::default();
        let mut lengths = Vec::new();
        for chain_index in 0..chains {
            let mut chain =
                ChainGenerator::new(seed * 1000 + chain_index, GeneratorOptions::default());
            let mut stop = String::new();
            while chain.blocks() < length {
                let request = chain.request().clone();
                let labels = chain.labels().to_vec();
                let response = match adapter.call(&request) {
                    Ok(response) => response,
                    Err(failure) => {
                        stop = format!("call: {failure}");
                        break;
                    }
                };
                reach.record(&request, &labels, &response);
                if let Err(error) = chain.next(&response) {
                    stop = format!("{error:#}");
                    break;
                }
            }
            let start = chain.request().header.number - chain.blocks();
            let schedule = Schedule::mainnet();
            println!(
                "chain {chain_index}: {} blocks from {start} ({:?} to {:?}; {} programs, {} tokens, {} policies) {stop}",
                chain.blocks(),
                Era::at(schedule.timestamp_parts(start).0),
                Era::at(chain.request().header.timestamp),
                chain.registry().programs.len(),
                chain.registry().tokens.len(),
                chain.registry().policies.len(),
            );
            lengths.push(chain.blocks());
        }
        reach.report("chains");
        println!("chain lengths {lengths:?}");
    }

    /// A gap that moves the pending block onto the Denim activation applies Denim's gas
    /// parameter scaling, and a later gap on the same block does not apply it again.
    #[test]
    fn gap_onto_denim_scales_gas_parameters_once() {
        let options = GeneratorOptions {
            eras: vec![Era::Cobalt],
            activation_percent: 0,
            ..GeneratorOptions::default()
        };
        let denim = Era::Denim.activation_timestamp();
        for seed in 0..4 {
            let mut chain = ChainGenerator::new(seed, options.clone());
            let timestamp = chain.request().header.timestamp;
            assert!(timestamp < denim);
            let scaled = (chain.request().header.gas_limit / 10).max(5_000_000);
            assert!(chain.edit(ChainEdit::Gap { secs: denim - timestamp }));
            assert_eq!(chain.request().header.timestamp, denim);
            assert_eq!(chain.request().header.gas_limit, scaled);
            assert!(chain.labels().iter().any(|label| label == "baseTime"));
            assert!(chain.edit(ChainEdit::Gap { secs: 1 }));
            assert_eq!(chain.request().header.timestamp, denim + 1);
            assert_eq!(chain.request().header.gas_limit, scaled);
        }
    }
}
