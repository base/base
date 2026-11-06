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
//!   from its bundle, then advancing validity dormancy, which checks the builder's tip and
//!   classifies the transactions admitted since the last classification). It runs off the build
//!   path on a node, so it is reported beside `total`.
//! - `intra_mutation`: pruning committed transactions and advancing their senders between
//!   flashblocks, as the Flashblocks builder does. Zero for the native builder.
//! - `held_drop`: dropping the previous block's probe iterator, which the held-iterator case keeps
//!   alive across `mutation` so an obsolete pool snapshot is torn down here.
//! - `carry`: starting the build: beginning a build against the pool's dormancy index on the
//!   block's parent, which applies the pre-selection writes or discards the index when the parent
//!   is not the tip, or else taking the builder's resting set from its store and carrying it into
//!   the block.
//! - `probe_*`: a diagnostic iterator created after the real build iterator, then asked for its
//!   first candidate, drained without commits, and dropped. The build's `construct` phase pays
//!   the first view sync; the probe must not perform it ahead of the measured build. Its yield
//!   count shows whether resting lanes reach the iterator at all. Both arms are probe-warmed.
//! - `construct`: building the build's pool iterators, one per flashblock.
//! - `selection`: the build loop, which includes executing the included transactions.
//! - `store`: sealing the block: making it the pool's dormancy tip, or putting the builder's
//!   resting set back for the next block.
//! - `build_drop`: dropping the block's adapter and its last pool iterator after `store`. The
//!   Flashblocks builder drops each earlier iterator inside `construct`, when the next flashblock
//!   replaces it; the native builder's selection loop consumes its iterator, so its drop falls in
//!   `selection` and `build_drop` is zero.
//!
//! `total` is `carry + construct + selection + store + build_drop`, the build path a
//! payload job pays; mutation, dormancy maintenance and probes are reported beside it, never inside
//! it. Each row also reports `dormant`, the pool's dormant-transaction gauge as the block's build
//! saw it. A block whose probe yields more than the block's executable transactions and the
//! resting transactions admitted since the last classification is still classifying resting
//! candidates and is reported apart from steady-state blocks, so the initial classification is
//! visible. A block built on a parent other than the last sealed block is reported apart too, so
//! the cost of recovering from a discarded index is visible.
//!
//! Timings from a laptop are not evidence of production latency; compare a baseline and a
//! candidate build on the same host.

use std::{
    collections::VecDeque,
    ops::Range,
    time::{Duration, Instant},
};

use alloy_primitives::{B256, U256, keccak256};
use base_execution_payload_builder::RestingPredicateMode;
use base_execution_txpool::{ValidityDormancyMode, ValidityOperator, ValidityPredicate};
use revm::{
    database::{AccountStatus, InMemoryDB, TransitionAccount, TransitionState},
    state::AccountInfo,
};

use super::production_pool::{
    ADMISSION_BATCH, Accounts, BLOCK_TIME_SECS, NONCE_FREE_LIFETIME_SECS, RestingKind,
    valid_through_block, watched_balance_is_one,
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
/// Percent of the churn case's lanes replaced by new resting lanes each block, by default.
pub const CHURN_REPLACED_PERCENT: usize = 5;
/// Percent of the churn case's lanes satisfied by a commit in each block, by default.
pub const CHURN_SATISFIED_PERCENT: usize = 1;
/// The discard case builds every this many blocks on a parent that is not the last sealed block,
/// by default.
pub const DISCARD_INTERVAL: u64 = 4;
/// Parked population of the demo case by default.
pub const DEMO_POPULATION: usize = 1_000_000;
/// Parked validity transactions the demo admits every block, by default: 10,000 a second at
/// [`DEMO_BLOCK_INTERVAL`].
pub const DEMO_ARRIVALS: usize = 2_000;
/// Validity transactions valid at admission the demo admits and includes every block, by default.
pub const DEMO_VALID: usize = 200;
/// Parked validity transactions a commit in the same block satisfies, every block, by default.
pub const DEMO_SATISFIED: usize = 100;
/// Distinct watched balances the demo's parked predicates spread over, by default.
pub const DEMO_WATCHED: usize = 10_000;
/// Percent of the demo's parked transactions that watch one hot balance, by default.
pub const DEMO_HOT_PERCENT: usize = 10;
/// Block interval the demo's ingest rate is reported at.
pub const DEMO_BLOCK_INTERVAL: Duration = Duration::from_millis(200);
/// Build-path time per block the demo counts blocks over.
pub const DEMO_BUILD_BUDGET: Duration = Duration::from_millis(50);
/// Parked validity transactions each concurrent demo submitter admits per call of the pool's
/// async admission API, by default.
pub const DEMO_SUBMIT_BATCH: usize = 256;

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
    /// Protocol lanes, a share replaced by new resting lanes and a share satisfied by a commit in
    /// the same block, every block after the first; see [`Churn`].
    Churn,
    /// Protocol lanes, every [`Matrix::discard_interval`]th block built on a parent that is not
    /// the last sealed block, which discards what the pool or the builder carried.
    Discard,
    /// A large parked population, classified off the build path before the first block, under
    /// production-like ingest every block; see [`Demo`]. Runs only when selected.
    Demo,
}

impl Case {
    /// Every case, in report order.
    pub const ALL: [Self; 9] = [
        Self::Protocol,
        Self::HeldIterator,
        Self::Descendants,
        Self::Channel,
        Self::NonceFree,
        Self::Liveness,
        Self::Churn,
        Self::Discard,
        Self::Demo,
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
            Self::Churn => "churn",
            Self::Discard => "discard",
            Self::Demo => "demo",
        }
    }

    /// The lane kind the case rests.
    pub const fn kind(self) -> RestingKind {
        match self {
            Self::Protocol
            | Self::HeldIterator
            | Self::Liveness
            | Self::Churn
            | Self::Discard
            | Self::Demo => RestingKind::Protocol,
            Self::Descendants => RestingKind::ProtocolDescendants,
            Self::Channel => RestingKind::Channel,
            Self::NonceFree => RestingKind::NonceFree,
        }
    }

    /// Whether each block's probe iterator outlives the next block's pool maintenance.
    pub const fn holds_iterator(self) -> bool {
        matches!(self, Self::HeldIterator)
    }

    /// Whether the case runs when `RESTING_MATRIX_CASES` is unset.
    pub const fn runs_by_default(self) -> bool {
        !matches!(self, Self::Demo)
    }

    /// Populations the case runs by default.
    pub fn default_populations(self) -> Vec<usize> {
        match self {
            Self::Protocol | Self::HeldIterator => POPULATIONS.to_vec(),
            Self::Descendants
            | Self::Channel
            | Self::NonceFree
            | Self::Liveness
            | Self::Churn
            | Self::Discard => vec![FOCUSED_POPULATION],
            Self::Demo => vec![DEMO_POPULATION],
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

    /// Lanes replaced and satisfied each block among `lanes` lanes.
    pub const fn churn(self, churn: Churn, lanes: usize) -> ChurnLanes {
        if matches!(self, Self::Churn) {
            ChurnLanes {
                replaced: lanes * churn.replaced_percent / 100,
                satisfied: lanes * churn.satisfied_percent / 100,
            }
        } else {
            ChurnLanes { replaced: 0, satisfied: 0 }
        }
    }

    /// Whether `block` builds on a parent that is not the last sealed block.
    pub const fn discards(self, block: u64, interval: u64) -> bool {
        matches!(self, Self::Discard) && block.is_multiple_of(interval)
    }

    /// Transactions beyond the ordinary traffic a block of the case may include: every woken lane,
    /// or every satisfied churn lane and the transfer that satisfies it.
    pub const fn headroom(self, churn: ChurnLanes) -> usize {
        let satisfied = 2 * churn.satisfied;
        if satisfied > WOKEN { satisfied } else { WOKEN }
    }

    /// First block by which the pool must have expired the expiring lanes.
    pub const fn expired_by(self) -> Option<u64> {
        if matches!(self, Self::Liveness) { Some(EXPIRY_DEADLINE + 2) } else { None }
    }

    /// Panics unless `population` over `blocks` blocks fits the case, so a bad override fails
    /// before anything is admitted: populations are whole lanes, the liveness case has every lane
    /// it wakes and expires and runs past its wake and expiry, nonce-free lanes outlive the run,
    /// churn shares fit the population, and the discard case discards at least once, with blocks
    /// that follow the last sealed block in between.
    pub fn check(self, population: usize, blocks: u64, churn: Churn, discard_interval: u64) {
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
        if matches!(self, Self::Churn) {
            assert!(
                churn.replaced_percent + churn.satisfied_percent <= 100,
                "the churn case replaces and satisfies at most every lane each block"
            );
        }
        if matches!(self, Self::Discard) {
            assert!(
                discard_interval >= MIN_BLOCKS && blocks >= discard_interval,
                "the discard case discards every {discard_interval} blocks, at least every \
                 {MIN_BLOCKS} and at least once in {blocks} blocks"
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

/// Shares of the churn case's lanes that change each block, in percent of its lanes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Churn {
    /// Lanes removed from the pool and replaced by as many new resting lanes.
    pub replaced_percent: usize,
    /// Lanes whose predicate a transfer in the block satisfies, so the block includes them; as
    /// many new resting lanes replace them.
    pub satisfied_percent: usize,
}

/// Lanes of one case that change each block.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChurnLanes {
    /// Lanes removed from the pool and replaced by new resting lanes.
    pub replaced: usize,
    /// Lanes satisfied, and included, in the block, then replaced by new resting lanes.
    pub satisfied: usize,
}

impl ChurnLanes {
    /// Takes the oldest of `live` lanes to replace and to satisfy this block.
    pub fn take(&self, live: &mut VecDeque<usize>) -> (Vec<usize>, Vec<usize>) {
        let replaced = live.drain(..self.replaced).collect();
        let satisfied = live.drain(..self.satisfied).collect();
        (replaced, satisfied)
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

/// Per-block ingest and predicate layout of the demo case. Every validity transaction comes from
/// its own [`Accounts::Validity`] sender, numbered in admission order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Demo {
    /// Parked validity transactions admitted every block: the first half before the previous
    /// block's canonical maintenance, which classifies them, and the rest after it, so they reach
    /// the build unclassified.
    pub arrivals: usize,
    /// Validity transactions whose predicates hold at admission, admitted after the previous
    /// block's maintenance and included in the block.
    pub valid: usize,
    /// Parked validity transactions admitted before the previous block's maintenance, each
    /// watching a balance of its own that a transfer admitted for the block funds; the block
    /// includes both.
    pub satisfied: usize,
    /// Distinct watched balances the parked transactions' predicates spread over.
    pub watched: usize,
    /// Percent of parked transactions that watch one hot balance instead.
    pub hot_percent: usize,
}

impl Demo {
    /// Predicates of the parked transaction from `sender`, which never hold: one of
    /// [`Self::watched`] unfunded balances, or the hot one for [`Self::hot_percent`] of senders.
    pub fn parked_predicates(&self, sender: usize) -> Vec<ValidityPredicate> {
        let hot = self.watched;
        let watched = if sender % 100 < self.hot_percent { hot } else { sender % self.watched };
        vec![watched_balance_is_one(watched)]
    }

    /// The watched balance of the `target`th same-block-satisfied transaction, apart from the
    /// parked transactions' balances so its satisfier wakes it alone.
    pub const fn target_watched(&self, target: usize) -> usize {
        self.watched + 1 + target
    }

    /// Predicates of a validity transaction from `sender` that hold at admission and in the
    /// build: the funded sender's balance is positive.
    pub fn valid_predicates(sender: usize) -> Vec<ValidityPredicate> {
        vec![ValidityPredicate::Balance {
            address: Accounts::Validity.address(sender),
            op: ValidityOperator::GreaterThan,
            value: U256::ZERO,
        }]
    }

    /// Arrivals admitted before the previous block's maintenance.
    pub const fn early_arrivals(&self) -> usize {
        self.arrivals / 2
    }

    /// Arrivals admitted after the previous block's maintenance, which reach the build
    /// unclassified.
    pub const fn late_arrivals(&self) -> usize {
        self.arrivals - self.early_arrivals()
    }

    /// Parked arrivals per second at [`DEMO_BLOCK_INTERVAL`].
    pub fn ingest_per_second(&self) -> f64 {
        self.arrivals as f64 / DEMO_BLOCK_INTERVAL.as_secs_f64()
    }
}

/// The demo's setup: admitting its parked population and classifying it before the first block,
/// with the process's resident set size after each step.
#[derive(Debug, Clone, Copy, Default)]
pub struct DemoSetup {
    /// Parked transactions admitted.
    pub parked: usize,
    /// Funding the parked senders in the state the pool validates against.
    pub funding: Duration,
    /// Generating and admitting the parked transactions.
    pub admission: Duration,
    /// The canonical maintenance that classified them, `None` when the pool keeps no dormancy
    /// index.
    pub classification: Option<Duration>,
    /// Resident bytes before funding.
    pub rss_base: Option<u64>,
    /// Resident bytes after funding.
    pub rss_funded: Option<u64>,
    /// Resident bytes after admission.
    pub rss_admitted: Option<u64>,
    /// Resident bytes after classification.
    pub rss_classified: Option<u64>,
}

impl DemoSetup {
    /// The classification's `classification_ms` and `classification_tx_per_s` report fields.
    fn classification_fields(&self) -> String {
        self.classification.map_or_else(
            || "classification_ms=na classification_tx_per_s=na".to_owned(),
            |duration| {
                format!(
                    "classification_ms={:.3} classification_tx_per_s={:.0}",
                    duration.as_secs_f64() * 1e3,
                    self.parked as f64 / duration.as_secs_f64()
                )
            },
        )
    }
}

/// One demo block's ingest and inclusion counts, beside its [`BlockTiming`].
#[derive(Debug, Clone, Copy, Default)]
pub struct DemoBlock {
    /// Parked arrivals admitted for the block.
    pub arrivals: usize,
    /// Predicates the build loop found unsatisfied: parked transactions that reached the build
    /// before any classification.
    pub parked_evaluated: usize,
    /// Validity transactions valid at admission the block included.
    pub valid_included: usize,
    /// Same-block-satisfied validity transactions the block included.
    pub satisfied_included: usize,
    /// The pool's canonical maintenance between the previous build and this one: the canonical
    /// update and the state-diff maintenance, which classifies, without admission.
    pub maintenance: Duration,
}

/// Concurrent ingest for the demo case: after setup, [`Self::submitters`] threads keep admitting
/// parked validity transactions through the pool's async admission API while the blocks build on
/// a [`DEMO_BLOCK_INTERVAL`] cadence and each sealed block's canonical maintenance runs on a
/// thread of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DemoConcurrency {
    /// Submitter threads, each admitting from senders of its own.
    pub submitters: usize,
    /// Transactions per admission call.
    pub batch: usize,
    /// Aggregate admission rate the submitters pace to, in transactions a second, or `None` to
    /// admit as fast as the pool accepts.
    pub rate: Option<u64>,
}

impl DemoConcurrency {
    /// Time between the starts of one submitter's batches at the target rate, `None` when
    /// free-running.
    pub fn batch_interval(&self) -> Option<Duration> {
        self.rate.map(|rate| {
            Duration::from_secs_f64((self.batch * self.submitters) as f64 / rate as f64)
        })
    }
}

/// One block of the concurrent demo, built while submitters admit and the previous block's
/// maintenance may still run.
#[derive(Debug, Clone, Copy, Default)]
pub struct ConcurrentBlock {
    /// The build-path phases (`carry`, `construct`, `selection`, `store`), block number,
    /// inclusions, and the dormant gauge when the build began.
    pub timing: BlockTiming,
    /// Funding and admitting the block's valid, satisfied, ordinary and satisfying transactions
    /// on the build thread just before the build, contending with the submitters.
    pub prep: Duration,
    /// Parked transactions the submitters admitted during the block's slot.
    pub ingested: usize,
    /// Predicates the build loop found unsatisfied: parked transactions that reached the build
    /// before any classification.
    pub parked_evaluated: usize,
    /// Whether the build ran without the pool's dormancy index although the pool enforces
    /// dormancy.
    pub fallback: bool,
    /// Whether the block's prep and build overran its slot, so the next block started at once.
    pub overran: bool,
    /// The block's canonical maintenance on the maintenance thread: the canonical update and the
    /// state-diff maintenance, which classifies.
    pub maintenance: Duration,
    /// How long the block's maintenance ran on after the next block's build began; zero when it
    /// finished first or no block followed.
    pub maintenance_lag: Duration,
}

/// What the concurrent demo's submitters did while the blocks built.
#[derive(Debug, Clone, Default)]
pub struct ConcurrentIngest {
    /// Parked transactions admitted from the first block's slot until the last block ended.
    pub admitted: usize,
    /// From the first block's slot until the last block ended.
    pub elapsed: Duration,
    /// Latency of every admission call, each carrying [`DemoConcurrency::batch`] transactions.
    pub batches: Vec<Duration>,
    /// The pool's dormant gauge once the last block's maintenance finished.
    pub dormant_end: usize,
}

/// The matrix to run, from the environment:
///
/// - `RESTING_MATRIX_CASES`: comma-separated [`Case::label`]s, default every case that
///   [`Case::runs_by_default`].
/// - `RESTING_MATRIX_POPULATIONS`: comma-separated populations replacing every case's default.
/// - `RESTING_MATRIX_BLOCKS`: blocks per case, at least [`MIN_BLOCKS`], default [`BLOCKS`].
/// - `RESTING_MATRIX_MODES`: comma-separated [`Mode::label`]s, default `enforce`.
/// - `RESTING_SWEEP_BENCH_GAP_MS`: untimed pause between blocks, standing in for the state root,
///   default none.
/// - `RESTING_MATRIX_CHURN_PERCENT`: [`Churn::replaced_percent`], default
///   [`CHURN_REPLACED_PERCENT`].
/// - `RESTING_MATRIX_SATISFIED_PERCENT`: [`Churn::satisfied_percent`], default
///   [`CHURN_SATISFIED_PERCENT`].
/// - `RESTING_MATRIX_DISCARD_INTERVAL`: [`Matrix::discard_interval`], default
///   [`DISCARD_INTERVAL`].
/// - `RESTING_MATRIX_DEMO_ARRIVALS`, `RESTING_MATRIX_DEMO_VALID`, `RESTING_MATRIX_DEMO_SATISFIED`,
///   `RESTING_MATRIX_DEMO_WATCHED`, `RESTING_MATRIX_DEMO_HOT_PERCENT`: the [`Demo`] fields,
///   defaults [`DEMO_ARRIVALS`], [`DEMO_VALID`], [`DEMO_SATISFIED`], [`DEMO_WATCHED`] and
///   [`DEMO_HOT_PERCENT`].
/// - `RESTING_MATRIX_DEMO_SUBMITTERS`: [`DemoConcurrency::submitters`]; set to at least one to run
///   the demo concurrently, unset to run it sequentially.
/// - `RESTING_MATRIX_DEMO_BATCH`: [`DemoConcurrency::batch`], at most [`ADMISSION_BATCH`], default
///   [`DEMO_SUBMIT_BATCH`].
/// - `RESTING_MATRIX_DEMO_RATE`: [`DemoConcurrency::rate`], default free-running.
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
    /// Lanes the churn case changes each block.
    pub churn: Churn,
    /// The discard case builds every this many blocks on a parent other than the last sealed
    /// block.
    pub discard_interval: u64,
    /// Per-block ingest and predicate layout of the demo case.
    pub demo: Demo,
    /// Concurrent ingest for the demo case, `None` to run it sequentially.
    pub demo_concurrency: Option<DemoConcurrency>,
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
            .unwrap_or_else(|| {
                Case::ALL.into_iter().filter(|case| case.runs_by_default()).collect()
            })
            .into_iter()
            .map(|case| (case, populations.clone().unwrap_or_else(|| case.default_populations())))
            .collect();
        let number = |name: &str, default: u64| {
            std::env::var(name).map_or(default, |value| {
                value.parse().unwrap_or_else(|_| panic!("{name} is a number"))
            })
        };
        let blocks = number("RESTING_MATRIX_BLOCKS", BLOCKS);
        assert!(blocks >= MIN_BLOCKS, "the resting matrix builds at least {MIN_BLOCKS} blocks");
        let churn = Churn {
            replaced_percent: number("RESTING_MATRIX_CHURN_PERCENT", CHURN_REPLACED_PERCENT as u64)
                as usize,
            satisfied_percent: number(
                "RESTING_MATRIX_SATISFIED_PERCENT",
                CHURN_SATISFIED_PERCENT as u64,
            ) as usize,
        };
        let discard_interval = number("RESTING_MATRIX_DISCARD_INTERVAL", DISCARD_INTERVAL);
        for (case, populations) in &cases {
            for &population in populations {
                case.check(population, blocks, churn, discard_interval);
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
        let demo = Demo {
            arrivals: number("RESTING_MATRIX_DEMO_ARRIVALS", DEMO_ARRIVALS as u64) as usize,
            valid: number("RESTING_MATRIX_DEMO_VALID", DEMO_VALID as u64) as usize,
            satisfied: number("RESTING_MATRIX_DEMO_SATISFIED", DEMO_SATISFIED as u64) as usize,
            watched: number("RESTING_MATRIX_DEMO_WATCHED", DEMO_WATCHED as u64) as usize,
            hot_percent: number("RESTING_MATRIX_DEMO_HOT_PERCENT", DEMO_HOT_PERCENT as u64)
                as usize,
        };
        assert!(demo.watched > 0, "the demo watches at least one balance");
        assert!(demo.hot_percent <= 100, "the demo's hot share is a percentage");
        let demo_concurrency = std::env::var("RESTING_MATRIX_DEMO_SUBMITTERS").ok().map(|_| {
            let concurrency = DemoConcurrency {
                submitters: number("RESTING_MATRIX_DEMO_SUBMITTERS", 0) as usize,
                batch: number("RESTING_MATRIX_DEMO_BATCH", DEMO_SUBMIT_BATCH as u64) as usize,
                rate: std::env::var("RESTING_MATRIX_DEMO_RATE")
                    .ok()
                    .map(|_| number("RESTING_MATRIX_DEMO_RATE", 0)),
            };
            assert!(concurrency.submitters > 0, "the concurrent demo runs at least one submitter");
            assert!(
                (1..=ADMISSION_BATCH).contains(&concurrency.batch),
                "a submitter batch fits one admission call of at most {ADMISSION_BATCH}"
            );
            assert_ne!(concurrency.rate, Some(0), "the concurrent demo's target rate is positive");
            concurrency
        });
        Self { cases, blocks, modes, gap, churn, discard_interval, demo, demo_concurrency }
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

/// Hash of a block at `block`'s height that the bench never sealed: a sibling of
/// [`block_hash`]`(block)` a build can name as its parent.
pub fn sibling_hash(block: u64) -> B256 {
    keccak256([block.to_be_bytes(), *b"sibling!"].concat())
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
    /// The block was built on a parent other than the last sealed block, so nothing carried over.
    Discard,
}

impl Phase {
    /// The phase of a block whose probe yielded `yielded` candidates while `expected` of the
    /// pool's transactions could be included or were admitted since the withholding mode last
    /// classified resting transactions.
    pub const fn of(
        mode: Mode,
        yielded: usize,
        expected: usize,
        woke: bool,
        discarded: bool,
    ) -> Self {
        if !mode.withholds() {
            Self::Off
        } else if discarded {
            Self::Discard
        } else if woke {
            Self::Wake
        } else if yielded > expected {
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
            Self::Discard => "discard",
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
    /// The pool's dormant validity transactions once the block's build began.
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
    /// Starting the build against the pool's dormancy index or carrying the resting set into it.
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
    /// Sealing the block's dormancy build or storing the resting set.
    pub store: Duration,
    /// Dropping the block's adapter and its last pool iterator.
    pub build_drop: Duration,
}

impl BlockTiming {
    /// The build path a payload job pays: carry, iterator construction, selection, store, and
    /// dropping the block's iterator.
    pub fn total(&self) -> Duration {
        self.carry + self.construct + self.selection + self.store + self.build_drop
    }

    /// Every metric in report order. Metrics added after `total` keep earlier columns in place.
    fn metrics(&self) -> [(&'static str, Duration); 14] {
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
        println!("{}", self.row_line(case, mode, population, timing));
    }

    /// Reports one demo block: its [`Self::row`] followed by its ingest and inclusion counts.
    pub fn demo_row(&self, mode: Mode, population: usize, timing: &BlockTiming, demo: &DemoBlock) {
        println!(
            "{} arrivals={} parked_evaluated={} valid_included={} satisfied_included={} \
             maint_ms={:.3}",
            self.row_line(Case::Demo, mode, population, timing),
            demo.arrivals,
            demo.parked_evaluated,
            demo.valid_included,
            demo.satisfied_included,
            demo.maintenance.as_secs_f64() * 1e3,
        );
    }

    fn row_line(&self, case: Case, mode: Mode, population: usize, timing: &BlockTiming) -> String {
        let metrics: String = timing
            .metrics()
            .iter()
            .map(|(name, value)| format!(" {name}_ms={:.3}", value.as_secs_f64() * 1e3))
            .collect();
        format!(
            "ROW {} block={} phase={} members={} probe_yielded={} included={}{metrics} dormant={}",
            self.key(case, mode, population),
            timing.block,
            timing.phase.label(),
            timing.members,
            timing.probe_yielded,
            timing.included,
            timing.dormant,
        )
    }

    /// Reports the demo's setup: admission and classification throughput, and resident memory
    /// after each step, in total and per parked transaction.
    pub fn demo_setup(&self, mode: Mode, population: usize, setup: &DemoSetup) {
        let per_second = |duration: Duration| setup.parked as f64 / duration.as_secs_f64();
        println!(
            "SETUP {} parked={} funding_ms={:.3} admission_ms={:.3} admission_tx_per_s={:.0} \
             {} rss_base_mb={} \
             rss_funded_mb={} rss_admitted_mb={} rss_classified_mb={} provider_bytes_per_tx={} \
             pool_bytes_per_tx={} dormancy_bytes_per_tx={} total_bytes_per_tx={}",
            self.key(Case::Demo, mode, population),
            setup.parked,
            setup.funding.as_secs_f64() * 1e3,
            setup.admission.as_secs_f64() * 1e3,
            per_second(setup.admission),
            setup.classification_fields(),
            mebibytes(setup.rss_base),
            mebibytes(setup.rss_funded),
            mebibytes(setup.rss_admitted),
            mebibytes(setup.rss_classified),
            bytes_per(setup.rss_base, setup.rss_funded, setup.parked),
            bytes_per(setup.rss_funded, setup.rss_admitted, setup.parked),
            bytes_per(setup.rss_admitted, setup.rss_classified, setup.parked),
            bytes_per(setup.rss_base, setup.rss_classified, setup.parked),
        );
    }

    /// Reports the demo's build-path percentiles over every block, the blocks over
    /// [`DEMO_BUILD_BUDGET`], mean maintenance, sustained ingest, setup throughput, and resident
    /// memory after setup and after the last block.
    pub fn demo_summary(
        &self,
        mode: Mode,
        population: usize,
        demo: &Demo,
        setup: &DemoSetup,
        blocks: &[(BlockTiming, DemoBlock)],
        rss_end: Option<u64>,
    ) {
        let mut builds: Vec<f64> =
            blocks.iter().map(|(timing, _)| timing.total().as_secs_f64() * 1e3).collect();
        builds.sort_by(f64::total_cmp);
        let over_budget =
            blocks.iter().filter(|(timing, _)| timing.total() > DEMO_BUILD_BUDGET).count();
        let maintenance =
            blocks.iter().map(|(_, block)| block.maintenance.as_secs_f64() * 1e3).sum::<f64>()
                / blocks.len() as f64;
        let parked_evaluated =
            blocks.iter().map(|(_, block)| block.parked_evaluated).sum::<usize>();
        println!(
            "DEMO {} blocks={} build_p50_ms={:.3} build_p99_ms={:.3} build_max_ms={:.3} \
             over_{}ms_blocks={over_budget} maint_mean_ms={maintenance:.3} \
             ingest_tx_per_s={:.0} parked_evaluated_total={parked_evaluated} \
             admission_tx_per_s={:.0} {} rss_classified_mb={} \
             rss_end_mb={} total_bytes_per_tx={}",
            self.key(Case::Demo, mode, population),
            blocks.len(),
            quantile(&builds, 0.5),
            quantile(&builds, 0.99),
            quantile(&builds, 1.0),
            DEMO_BUILD_BUDGET.as_millis(),
            demo.ingest_per_second(),
            setup.parked as f64 / setup.admission.as_secs_f64(),
            setup.classification_fields(),
            mebibytes(setup.rss_classified),
            mebibytes(rss_end),
            bytes_per(setup.rss_base, setup.rss_classified, setup.parked),
        );
    }

    /// Reports one concurrent demo block: its build path and phases, the prep before it, its
    /// maintenance and how far that ran into the next build, and the parked transactions the
    /// submitters admitted during its slot.
    pub fn concurrent_row(&self, mode: Mode, population: usize, block: &ConcurrentBlock) {
        let ms = |duration: Duration| duration.as_secs_f64() * 1e3;
        let timing = &block.timing;
        println!(
            "CROW {} block={} build_ms={:.3} carry_ms={:.3} construct_ms={:.3} selection_ms={:.3} \
             store_ms={:.3} prep_ms={:.3} maint_ms={:.3} maint_lag_ms={:.3} ingested={} \
             parked_evaluated={} included={} dormant={} fallback={} overran={}",
            self.key(Case::Demo, mode, population),
            timing.block,
            ms(timing.total()),
            ms(timing.carry),
            ms(timing.construct),
            ms(timing.selection),
            ms(timing.store),
            ms(block.prep),
            ms(block.maintenance),
            ms(block.maintenance_lag),
            block.ingested,
            block.parked_evaluated,
            timing.included,
            timing.dormant,
            block.fallback,
            block.overran,
        );
    }

    /// Reports the concurrent demo's achieved ingest against its target, admission-call latency
    /// per batch and per transaction, build-path percentiles and blocks over
    /// [`DEMO_BUILD_BUDGET`], maintenance percentiles and lag, the final dormant count, and
    /// resident memory after the last block.
    pub fn concurrent_summary(
        &self,
        mode: Mode,
        population: usize,
        concurrency: &DemoConcurrency,
        ingest: &ConcurrentIngest,
        blocks: &[ConcurrentBlock],
        rss_end: Option<u64>,
    ) {
        let sorted_ms = |values: &mut dyn Iterator<Item = Duration>| {
            let mut values: Vec<f64> = values.map(|value| value.as_secs_f64() * 1e3).collect();
            values.sort_by(f64::total_cmp);
            values
        };
        let batches = sorted_ms(&mut ingest.batches.iter().copied());
        let per_tx_us = |batch_ms: f64| batch_ms * 1e3 / concurrency.batch as f64;
        let builds = sorted_ms(&mut blocks.iter().map(|block| block.timing.total()));
        let maintenance = sorted_ms(&mut blocks.iter().map(|block| block.maintenance));
        let lags = sorted_ms(&mut blocks.iter().map(|block| block.maintenance_lag));
        let over_budget =
            blocks.iter().filter(|block| block.timing.total() > DEMO_BUILD_BUDGET).count();
        let lagged = blocks.iter().filter(|block| !block.maintenance_lag.is_zero()).count();
        let overruns = blocks.iter().filter(|block| block.overran).count();
        let fallbacks = blocks.iter().filter(|block| block.fallback).count();
        let batch_quantile = |q: f64| {
            if batches.is_empty() { f64::NAN } else { quantile(&batches, q) }
        };
        println!(
            "CONCURRENT {} blocks={} submitters={} batch={} target_tx_per_s={} \
             ingest_tx_per_s={:.0} ingested={} batches={} batch_p50_ms={:.3} batch_p99_ms={:.3} \
             batch_max_ms={:.3} tx_p50_us={:.2} tx_p99_us={:.2} tx_max_us={:.2} \
             build_p50_ms={:.3} build_p99_ms={:.3} build_max_ms={:.3} over_{}ms_blocks={over_budget} \
             overruns={overruns} fallback_builds={fallbacks} maint_p50_ms={:.3} maint_p99_ms={:.3} \
             maint_max_ms={:.3} maint_lagged_blocks={lagged} maint_lag_max_ms={:.3} dormant_end={} \
             rss_end_mb={}",
            self.key(Case::Demo, mode, population),
            blocks.len(),
            concurrency.submitters,
            concurrency.batch,
            concurrency.rate.map_or_else(|| "free".to_owned(), |rate| rate.to_string()),
            ingest.admitted as f64 / ingest.elapsed.as_secs_f64(),
            ingest.admitted,
            batches.len(),
            batch_quantile(0.5),
            batch_quantile(0.99),
            batch_quantile(1.0),
            per_tx_us(batch_quantile(0.5)),
            per_tx_us(batch_quantile(0.99)),
            per_tx_us(batch_quantile(1.0)),
            quantile(&builds, 0.5),
            quantile(&builds, 0.99),
            quantile(&builds, 1.0),
            DEMO_BUILD_BUDGET.as_millis(),
            quantile(&maintenance, 0.5),
            quantile(&maintenance, 0.99),
            quantile(&maintenance, 1.0),
            quantile(&lags, 1.0),
            ingest.dormant_end,
            mebibytes(rss_end),
        );
    }

    /// Reports the mean, median, 90th percentile, and maximum of every metric, per phase.
    pub fn summary(&self, case: Case, mode: Mode, population: usize, timings: &[BlockTiming]) {
        for phase in [Phase::Classify, Phase::Steady, Phase::Wake, Phase::Off, Phase::Discard] {
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
                let mean = values.iter().sum::<f64>() / values.len() as f64;
                line.push_str(&format!(
                    " {name}_mean_ms={mean:.3} {name}_p50_ms={:.3} {name}_p90_ms={:.3} {name}_max_ms={:.3}",
                    quantile(&values, 0.5),
                    quantile(&values, 0.9),
                    quantile(&values, 1.0),
                ));
            }
            println!("{line}");
        }
    }
}

/// The `q` quantile of ascending, non-empty `sorted`, by nearest rank.
fn quantile(sorted: &[f64], q: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * q).round() as usize]
}

/// `bytes` in mebibytes, or `na` when unmeasured.
fn mebibytes(bytes: Option<u64>) -> String {
    bytes.map_or_else(|| "na".to_owned(), |bytes| format!("{:.1}", bytes as f64 / (1 << 20) as f64))
}

/// Resident bytes gained from `before` to `after` per one of `count` items, or `na`.
fn bytes_per(before: Option<u64>, after: Option<u64>, count: usize) -> String {
    match (before, after) {
        (Some(before), Some(after)) if count > 0 => {
            format!("{:.0}", (after as f64 - before as f64) / count as f64)
        }
        _ => "na".to_owned(),
    }
}

/// The process's resident set size in bytes: `VmRSS` from `/proc/self/status` on Linux, `ps`
/// elsewhere, or `None` when neither is available.
pub fn resident_bytes() -> Option<u64> {
    let kibibytes = match std::fs::read_to_string("/proc/self/status") {
        Ok(status) => status
            .lines()
            .find_map(|line| line.strip_prefix("VmRSS:"))?
            .trim()
            .strip_suffix("kB")?
            .trim()
            .parse::<u64>()
            .ok()?,
        Err(_) => {
            let output = std::process::Command::new("ps")
                .args(["-o", "rss=", "-p", &std::process::id().to_string()])
                .output()
                .ok()?;
            String::from_utf8(output.stdout).ok()?.trim().parse::<u64>().ok()?
        }
    };
    Some(kibibytes * 1024)
}
