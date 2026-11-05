//! Resting-population matrix of the native resting-selection bench: the populations and lane
//! kinds it runs, the liveness cases, the modes that withhold resting transactions, the per-block
//! phase timings, and their report.
//!
//! Every case builds [`BLOCKS`] blocks by default on the production pool while the pool's base fee
//! changes from block to block. A block's timings are split into phases so no phase hides
//! another:
//!
//! - `mutation`: pool maintenance before the block (the canonical update that mines the previous
//!   block and moves the base fee, then admitting the block's ordinary traffic). Pool work, not
//!   selection.
//! - `dormancy`: the canonical state-diff maintenance for the previous block (guard invalidation
//!   from its bundle, then advancing validity dormancy, which applies queued builder reports, wakes
//!   and classifies). It runs off the build path on a node, so it is reported beside `total`.
//! - `intra_mutation`: pruning committed transactions and advancing their senders between
//!   flashblocks, as the Flashblocks builder does. Zero for the native builder.
//! - `held_drop`: dropping the previous block's probe iterator, which the held-iterator case keeps
//!   alive across `mutation` so an obsolete pool snapshot is torn down here.
//! - `carry`: taking the resting set from the store and carrying it into the block.
//! - `probe_*`: a diagnostic iterator created after the real build iterator, then asked for its
//!   first candidate, drained without commits, and dropped. The build's `construct` phase pays
//!   the first view sync; the probe must not perform it ahead of the measured build. Its yield
//!   count shows whether resting lanes reach the iterator at all. Both arms are probe-warmed.
//! - `construct`: building the build's pool iterators, one per flashblock.
//! - `selection`: the build loop, which includes executing the included transactions.
//! - `store`: putting the resting set back for the next block.
//! - `report`: the native builder's reports to the pool: the pre-selection writes, the validity
//!   transactions selection found unsatisfied, and the sealed block's state.
//! - `build_drop`: dropping the block's adapter and its last pool iterator after `store`. The
//!   Flashblocks builder drops each earlier iterator inside `construct`, when the next flashblock
//!   replaces it; the native builder's selection loop consumes its iterator, so its drop falls in
//!   `selection` and `build_drop` is zero.
//!
//! `total` is `carry + construct + selection + store + report + build_drop`, the build path a
//! payload job pays; mutation, dormancy maintenance and probes are reported beside it, never inside
//! it. Each row also reports `dormant`, the pool's dormant-transaction gauge as the block's build
//! saw it. A block whose probe yields more
//! than the block's executable transactions is still classifying resting candidates and is
//! reported apart from steady-state blocks, so the initial classification is visible.
//!
//! Timings from a laptop are not evidence of production latency; compare a baseline and a
//! candidate build on the same host.

use std::{
    ops::Range,
    time::{Duration, Instant},
};

use alloy_primitives::{B256, U256, keccak256};
use base_execution_payload_builder::RestingPredicateMode;
use base_execution_txpool::{ValidityDormancyMode, ValidityPredicate};
use revm::{
    database::{AccountStatus, InMemoryDB, TransitionAccount, TransitionState},
    state::AccountInfo,
};

use super::production_pool::{
    Accounts, BLOCK_TIME_SECS, NONCE_FREE_LIFETIME_SECS, RestingKind, valid_through_block,
    watched_balance_is_one,
};

/// Resting populations of the full matrix, in pool transactions.
pub const POPULATIONS: [usize; 4] = [0, 20_000, 40_000, 75_000];
/// Population of the focused lane-kind and liveness cases.
pub const FOCUSED_POPULATION: usize = 20_000;
/// Blocks each case builds by default.
pub const BLOCKS: u64 = 16;
/// Fewest blocks a case may build: one to classify and one to build after the first canonical
/// maintenance.
pub const MIN_BLOCKS: u64 = 2;
/// Block whose pre-block writes fund the watched accounts of the first [`WOKEN`] lanes.
pub const WAKE_BLOCK: u64 = 8;
/// Lanes the liveness case wakes at [`WAKE_BLOCK`].
pub const WOKEN: usize = 100;
/// Last block the expiring lanes of the liveness case are valid in.
pub const EXPIRY_DEADLINE: u64 = 4;
/// Lanes of the liveness case that expire after [`EXPIRY_DEADLINE`].
pub const EXPIRING: usize = 100;

/// One row of the matrix: a lane kind and how the bench drives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Case {
    /// Single-transaction protocol lanes, at every population.
    Protocol,
    /// [`Case::Protocol`], keeping each block's probe iterator alive across the next block's
    /// canonical fee update, at every population.
    HeldIterator,
    /// Protocol lanes whose gated head blocks plain successors.
    Descendants,
    /// EIP-8130 nonce channels.
    Channel,
    /// Nonce-free EIP-8130 transactions.
    NonceFree,
    /// Protocol lanes, some woken by a pre-block write and some expired by the pool.
    Liveness,
}

impl Case {
    /// Every case, in report order.
    pub const ALL: [Self; 6] = [
        Self::Protocol,
        Self::HeldIterator,
        Self::Descendants,
        Self::Channel,
        Self::NonceFree,
        Self::Liveness,
    ];

    /// The case's report label, also accepted by `RESTING_MATRIX_CASES`.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Protocol => "protocol",
            Self::HeldIterator => "held_iterator",
            Self::Descendants => "descendants",
            Self::Channel => "channel",
            Self::NonceFree => "nonce_free",
            Self::Liveness => "liveness",
        }
    }

    /// The lane kind the case rests.
    pub const fn kind(self) -> RestingKind {
        match self {
            Self::Protocol | Self::HeldIterator | Self::Liveness => RestingKind::Protocol,
            Self::Descendants => RestingKind::ProtocolDescendants,
            Self::Channel => RestingKind::Channel,
            Self::NonceFree => RestingKind::NonceFree,
        }
    }

    /// Whether each block's probe iterator outlives the next block's pool maintenance.
    pub const fn holds_iterator(self) -> bool {
        matches!(self, Self::HeldIterator)
    }

    /// Populations the case runs by default.
    pub fn default_populations(self) -> Vec<usize> {
        match self {
            Self::Protocol | Self::HeldIterator => POPULATIONS.to_vec(),
            Self::Descendants | Self::Channel | Self::NonceFree | Self::Liveness => {
                vec![FOCUSED_POPULATION]
            }
        }
    }

    /// Lanes holding `population` pool transactions.
    pub const fn lanes(self, population: usize) -> usize {
        population / self.kind().transactions_per_lane()
    }

    /// Predicates gating the head of `lane`: a balance predicate that never holds unless the
    /// lane is woken, plus a block deadline for the liveness case's expiring lanes.
    pub fn predicates(self, lane: usize) -> Vec<ValidityPredicate> {
        let mut predicates = vec![watched_balance_is_one(lane)];
        if self.expiring().contains(&lane) {
            predicates.push(valid_through_block(EXPIRY_DEADLINE));
        }
        predicates
    }

    /// Lanes woken by `block`'s pre-block writes.
    pub const fn woken(self, block: u64) -> Range<usize> {
        if matches!(self, Self::Liveness) && block == WAKE_BLOCK { 0..WOKEN } else { 0..0 }
    }

    /// Lanes the pool expires after [`EXPIRY_DEADLINE`].
    pub const fn expiring(self) -> Range<usize> {
        if matches!(self, Self::Liveness) { WOKEN..WOKEN + EXPIRING } else { 0..0 }
    }

    /// First block by which the pool must have expired the expiring lanes.
    pub const fn expired_by(self) -> Option<u64> {
        if matches!(self, Self::Liveness) { Some(EXPIRY_DEADLINE + 2) } else { None }
    }

    /// Panics unless `population` over `blocks` blocks fits the case, so a bad override fails
    /// before anything is admitted: populations are whole lanes, the liveness case has every lane
    /// it wakes and expires and runs past its wake and expiry, and nonce-free lanes outlive the
    /// run.
    pub fn check(self, population: usize, blocks: u64) {
        let per_lane = self.kind().transactions_per_lane();
        assert!(
            population.is_multiple_of(per_lane),
            "{} populations are whole lanes of {per_lane} transactions, got {population}",
            self.label()
        );
        if matches!(self, Self::Liveness) {
            assert!(
                blocks > WAKE_BLOCK.max(EXPIRY_DEADLINE + 1),
                "the liveness case runs past block {WAKE_BLOCK}, got {blocks} blocks"
            );
            assert!(
                self.lanes(population) >= WOKEN + EXPIRING,
                "the liveness case needs at least {} lanes, got {population} transactions",
                WOKEN + EXPIRING
            );
        }
        if matches!(self, Self::NonceFree) {
            assert!(
                blocks * BLOCK_TIME_SECS < NONCE_FREE_LIFETIME_SECS,
                "nonce-free lanes expire {NONCE_FREE_LIFETIME_SECS} s after admission; {blocks} \
                 blocks outlive them"
            );
        }
    }

    /// Resting members expected once classification is done and every wake and expiry of
    /// `lanes` lanes has happened.
    pub const fn final_members(self, lanes: usize) -> usize {
        if matches!(self, Self::Liveness) { lanes.saturating_sub(WOKEN + EXPIRING) } else { lanes }
    }
}

/// How a run withholds resting transactions from builds.
///
/// The `Debug` name is the report's `mode` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The pool keeps dormant validity transactions out of the best-transactions snapshot; the
    /// builder's resting predicates are off.
    Enforce,
    /// Neither the pool nor the builder withholds anything.
    Off,
    /// The builder carries resting predicates across blocks and withholds them; pool dormancy is
    /// off.
    BuilderEnforce,
}

impl Mode {
    /// The mode's `RESTING_MATRIX_MODES` label.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Enforce => "enforce",
            Self::Off => "off",
            Self::BuilderEnforce => "builder_enforce",
        }
    }

    /// The pool's validity dormancy mode.
    pub const fn dormancy(self) -> ValidityDormancyMode {
        match self {
            Self::Enforce => ValidityDormancyMode::Enforce,
            Self::Off | Self::BuilderEnforce => ValidityDormancyMode::Off,
        }
    }

    /// The builder's resting predicate mode.
    pub const fn resting(self) -> RestingPredicateMode {
        match self {
            Self::BuilderEnforce => RestingPredicateMode::Enforce,
            Self::Enforce | Self::Off => RestingPredicateMode::Off,
        }
    }

    /// Whether the pool or the builder withholds resting transactions.
    pub const fn withholds(self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// The matrix to run, from the environment:
///
/// - `RESTING_MATRIX_CASES`: comma-separated [`Case::label`]s, default all.
/// - `RESTING_MATRIX_POPULATIONS`: comma-separated populations replacing every case's default.
/// - `RESTING_MATRIX_BLOCKS`: blocks per case, at least [`MIN_BLOCKS`], default [`BLOCKS`].
/// - `RESTING_MATRIX_MODES`: comma-separated [`Mode::label`]s, default `enforce`.
/// - `RESTING_SWEEP_BENCH_GAP_MS`: untimed pause between blocks, standing in for the state root,
///   default none.
///
/// Every case and population is checked with [`Case::check`] before any pool is built.
#[derive(Debug, Clone)]
pub struct Matrix {
    /// Cases to run, each with its populations.
    pub cases: Vec<(Case, Vec<usize>)>,
    /// Blocks per case.
    pub blocks: u64,
    /// Modes to run every case under.
    pub modes: Vec<Mode>,
    /// Untimed pause between blocks.
    pub gap: Option<Duration>,
}

impl Matrix {
    /// Reads the matrix from the environment.
    pub fn from_env() -> Self {
        let list = |name: &str| {
            std::env::var(name)
                .ok()
                .map(|value| value.split(',').map(str::trim).map(str::to_owned).collect::<Vec<_>>())
        };
        let populations = list("RESTING_MATRIX_POPULATIONS").map(|values| {
            values
                .iter()
                .map(|value| value.parse().expect("RESTING_MATRIX_POPULATIONS holds numbers"))
                .collect::<Vec<usize>>()
        });
        let cases: Vec<(Case, Vec<usize>)> = list("RESTING_MATRIX_CASES")
            .map(|labels| {
                labels
                    .iter()
                    .map(|label| {
                        *Case::ALL
                            .iter()
                            .find(|case| case.label() == label)
                            .unwrap_or_else(|| panic!("unknown resting matrix case {label}"))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| Case::ALL.to_vec())
            .into_iter()
            .map(|case| (case, populations.clone().unwrap_or_else(|| case.default_populations())))
            .collect();
        let blocks = std::env::var("RESTING_MATRIX_BLOCKS")
            .map_or(BLOCKS, |value| value.parse().expect("RESTING_MATRIX_BLOCKS is a number"));
        assert!(blocks >= MIN_BLOCKS, "the resting matrix builds at least {MIN_BLOCKS} blocks");
        for (case, populations) in &cases {
            for &population in populations {
                case.check(population, blocks);
            }
        }
        let modes = list("RESTING_MATRIX_MODES")
            .map(|labels| {
                labels
                    .iter()
                    .map(|label| {
                        *[Mode::Enforce, Mode::Off, Mode::BuilderEnforce]
                            .iter()
                            .find(|mode| mode.label() == label)
                            .unwrap_or_else(|| panic!("unknown resting matrix mode {label}"))
                    })
                    .collect()
            })
            .unwrap_or_else(|| vec![Mode::Enforce]);
        let gap = std::env::var("RESTING_SWEEP_BENCH_GAP_MS").ok().map(|ms| {
            Duration::from_millis(ms.parse().expect("RESTING_SWEEP_BENCH_GAP_MS is a number"))
        });
        Self { cases, blocks, modes, gap }
    }
}

/// Base fee of block `block`'s parent, in wei. Cycles so consecutive blocks change fees in both
/// directions; every value is far below the transactions' fee cap.
pub const fn parent_base_fee(block: u64) -> u64 {
    const FEES: [u64; 4] = [1_000_000_000, 1_500_000_000, 1_250_000_000, 2_000_000_000];
    FEES[(block % 4) as usize]
}

/// Hash the bench finishes `block` as, which the next block names as its parent.
pub fn block_hash(block: u64) -> B256 {
    keccak256(block.to_be_bytes())
}

/// Funds the watched accounts of `woken` lanes with the one wei their predicate requires.
pub fn fund_woken(db: &mut InMemoryDB, woken: Range<usize>) {
    for lane in woken {
        db.insert_account_info(
            Accounts::Watched.address(lane),
            AccountInfo { balance: U256::ONE, ..Default::default() },
        );
    }
}

/// The pre-block writes that fund the watched accounts of `woken` lanes.
pub fn wake_transitions(woken: Range<usize>) -> TransitionState {
    let mut transitions = TransitionState::default();
    for lane in woken {
        transitions.transitions.insert(
            Accounts::Watched.address(lane),
            TransitionAccount {
                info: Some(AccountInfo { balance: U256::ONE, ..Default::default() }),
                status: AccountStatus::Changed,
                previous_info: Some(AccountInfo::default()),
                previous_status: AccountStatus::Loaded,
                storage: Default::default(),
                storage_was_destroyed: false,
            },
        );
    }
    transitions
}

/// Whether a block was still classifying resting candidates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Phase {
    /// The probe yielded resting candidates beyond the block's executable transactions.
    #[default]
    Classify,
    /// The probe yielded only the block's executable transactions.
    Steady,
    /// The block's pre-block writes woke resting lanes.
    Wake,
    /// Nothing withholds resting candidates, so every one is yielded every block.
    Off,
}

impl Phase {
    /// The phase of a block whose probe yielded `yielded` candidates while `executable` of the
    /// pool's transactions could be included.
    pub const fn of(mode: Mode, yielded: usize, executable: usize, woke: bool) -> Self {
        if !mode.withholds() {
            Self::Off
        } else if woke {
            Self::Wake
        } else if yielded > executable {
            Self::Classify
        } else {
            Self::Steady
        }
    }

    /// The phase's report label.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Classify => "classify",
            Self::Steady => "steady",
            Self::Wake => "wake",
            Self::Off => "off",
        }
    }
}

/// A standalone iterator built from the job's view, timed through its first candidate and drain.
#[derive(Debug)]
pub struct Probe<I> {
    /// The drained iterator, dropped or held by the caller.
    pub iterator: I,
    /// Building the iterator, including the view sync.
    pub construct: Duration,
    /// Yielding the first candidate.
    pub first_yield: Duration,
    /// Yielding every remaining candidate without commits.
    pub drain: Duration,
    /// Candidates yielded.
    pub yielded: usize,
}

impl<I: Iterator> Probe<I> {
    /// Builds an iterator with `build` and drains it.
    pub fn run(build: impl FnOnce() -> I) -> Self {
        let start = Instant::now();
        let mut iterator = build();
        let construct = start.elapsed();
        let start = Instant::now();
        let first = iterator.next();
        let first_yield = start.elapsed();
        let start = Instant::now();
        let rest = iterator.by_ref().count();
        let drain = start.elapsed();
        let yielded = usize::from(first.is_some()) + rest;
        Self { iterator, construct, first_yield, drain, yielded }
    }
}

/// Drops `value`, returning how long the drop took.
pub fn timed_drop<T>(value: T) -> Duration {
    let start = Instant::now();
    drop(value);
    start.elapsed()
}

/// One block's phase timings and outcomes.
#[derive(Debug, Clone, Copy, Default)]
pub struct BlockTiming {
    /// Block number.
    pub block: u64,
    /// Whether the block was still classifying.
    pub phase: Phase,
    /// Resting members the builder carried into the block.
    pub members: usize,
    /// The pool's dormant validity transactions when the block's build began.
    pub dormant: usize,
    /// Candidates the probe yielded.
    pub probe_yielded: usize,
    /// Transactions the block included.
    pub included: usize,
    /// Pool maintenance before the block.
    pub mutation: Duration,
    /// Canonical state-diff maintenance for the previous block, including validity dormancy.
    pub dormancy: Duration,
    /// Pool maintenance between flashblocks.
    pub intra_mutation: Duration,
    /// Dropping the previous block's held probe iterator.
    pub held_drop: Duration,
    /// Carrying the resting set into the block.
    pub carry: Duration,
    /// Building the probe iterator.
    pub probe_construct: Duration,
    /// The probe's first candidate.
    pub probe_first_yield: Duration,
    /// Draining the probe.
    pub probe_drain: Duration,
    /// Dropping the probe, zero when held.
    pub probe_drop: Duration,
    /// Building the build's pool iterators.
    pub construct: Duration,
    /// The build loop.
    pub selection: Duration,
    /// Storing the resting set.
    pub store: Duration,
    /// The builder's reports to the pool.
    pub report: Duration,
    /// Dropping the block's adapter and its last pool iterator.
    pub build_drop: Duration,
}

impl BlockTiming {
    /// The build path a payload job pays: carry, iterator construction, selection, store, the
    /// builder's reports, and dropping the block's iterator.
    pub fn total(&self) -> Duration {
        self.carry + self.construct + self.selection + self.store + self.report + self.build_drop
    }

    /// Every metric in report order. Metrics added after `total` keep earlier columns in place.
    fn metrics(&self) -> [(&'static str, Duration); 15] {
        [
            ("mutation", self.mutation),
            ("intra_mutation", self.intra_mutation),
            ("held_drop", self.held_drop),
            ("carry", self.carry),
            ("probe_construct", self.probe_construct),
            ("probe_first_yield", self.probe_first_yield),
            ("probe_drain", self.probe_drain),
            ("probe_drop", self.probe_drop),
            ("construct", self.construct),
            ("selection", self.selection),
            ("store", self.store),
            ("build_drop", self.build_drop),
            ("total", self.total()),
            ("dormancy", self.dormancy),
            ("report", self.report),
        ]
    }
}

/// Prints the matrix as `key=value` lines: `SETUP` per case, `ROW` per block, and `SUMMARY` per
/// phase.
#[derive(Debug, Clone, Copy)]
pub struct Report {
    /// The builder the bench drives.
    pub builder: &'static str,
}

impl Report {
    fn key(&self, case: Case, mode: Mode, population: usize) -> String {
        format!(
            "builder={} case={} mode={mode:?} population={population}",
            self.builder,
            case.label()
        )
    }

    /// Reports admitting a case's resting population, which precedes every block.
    pub fn setup(
        &self,
        case: Case,
        mode: Mode,
        population: usize,
        lanes: usize,
        admission: Duration,
    ) {
        println!(
            "SETUP {} lanes={lanes} admission_ms={:.3}",
            self.key(case, mode, population),
            admission.as_secs_f64() * 1e3
        );
    }

    /// Reports one block.
    pub fn row(&self, case: Case, mode: Mode, population: usize, timing: &BlockTiming) {
        let metrics: String = timing
            .metrics()
            .iter()
            .map(|(name, value)| format!(" {name}_ms={:.3}", value.as_secs_f64() * 1e3))
            .collect();
        println!(
            "ROW {} block={} phase={} members={} probe_yielded={} included={}{metrics} dormant={}",
            self.key(case, mode, population),
            timing.block,
            timing.phase.label(),
            timing.members,
            timing.probe_yielded,
            timing.included,
            timing.dormant,
        );
    }

    /// Reports the mean, median, 90th percentile, and maximum of every metric, per phase.
    pub fn summary(&self, case: Case, mode: Mode, population: usize, timings: &[BlockTiming]) {
        for phase in [Phase::Classify, Phase::Steady, Phase::Wake, Phase::Off] {
            let blocks: Vec<_> = timings.iter().filter(|timing| timing.phase == phase).collect();
            if blocks.is_empty() {
                continue;
            }
            let mut line = format!(
                "SUMMARY {} phase={} blocks={}",
                self.key(case, mode, population),
                phase.label(),
                blocks.len()
            );
            for index in 0..BlockTiming::default().metrics().len() {
                let name = BlockTiming::default().metrics()[index].0;
                let mut values: Vec<f64> = blocks
                    .iter()
                    .map(|timing| timing.metrics()[index].1.as_secs_f64() * 1e3)
                    .collect();
                values.sort_by(f64::total_cmp);
                let quantile = |q: f64| values[((values.len() - 1) as f64 * q).round() as usize];
                let mean = values.iter().sum::<f64>() / values.len() as f64;
                line.push_str(&format!(
                    " {name}_mean_ms={mean:.3} {name}_p50_ms={:.3} {name}_p90_ms={:.3} {name}_max_ms={:.3}",
                    quantile(0.5),
                    quantile(0.9),
                    quantile(1.0),
                ));
            }
            println!("{line}");
        }
    }
}
