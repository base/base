//! Flashblock build workloads for the builder performance gate.
//!
//! Each [`FlashblockWorkload`] describes one block of pool traffic: plain transfers that fill
//! every flashblock, a backlog of validity transactions whose predicates stay unsatisfied (so the
//! build loop re-considers, re-evaluates and re-parks them on every flashblock), arrivals between
//! flashblocks, and optional satisfied validity transactions. [`FlashblockWorkloadFixture`]
//! materializes a workload into a pool, an MDBX database whose genesis funds every sender, and a
//! builder context, then runs it through one of two builders:
//!
//! - [`FlashblockWorkloadFixture::run_block`] drives the flashblocks builder through
//!   [`FlashblockBlockDriver`], adding each flashblock's arrivals before it is built.
//! - [`FlashblockWorkloadFixture::run_native_block`] drives the native payload builder that
//!   serves Denim blocks: one pass over the pool for the whole block, with every arrival already
//!   in the pool and the block gas limit set to the workload's total gas target.
//!
//! Everything is deterministic: fixed senders, nonces, fees, and predicates, and the
//! wall-clock predicate evaluation cutoff is disabled so control flow does not depend on how
//! fast the host (or Valgrind) runs.

use std::{path::Path, sync::Arc, time::Duration};

use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_genesis::{Genesis, GenesisAccount};
use alloy_primitives::{Address, B256, Bytes, Signature, TxHash, TxKind, U256};
use alloy_rpc_types_engine::PayloadId;
use base_common_chains::BaseUpgrade;
use base_common_consensus::{BasePrimitives, BaseTransactionSigned, BaseTxEnvelope, Predeploys};
use base_common_evm::BaseTime;
use base_execution_chainspec::BaseChainSpec;
use base_execution_evm::BaseEvmConfig;
use base_execution_payload_builder::{
    builder::{BasePayloadBuilderCtx as NativePayloadBuilderCtx, Builder as NativeBuilder},
    config::BaseBuilderConfig,
    payload::{BasePayloadBuilderAttributes, EthPayloadBuilderAttributes},
};
use base_execution_txpool::{
    BaseOrdering, BasePooledTransaction, ParkedBestTransactions, ValidityOperator,
    ValidityPredicate,
};
use base_node_core::BaseNode;
use base_observability_events::{
    GlobalTransactionEventWriter, TransactionEventProducer, TransactionEventWriterConfig,
};
use reth_basic_payload_builder::{BuildOutcomeKind, PayloadConfig};
use reth_chainspec::{ChainSpec, ForkCondition};
use reth_db::{DatabaseEnv, test_utils::TempDatabase};
use reth_db_common::init::init_genesis;
use reth_node_api::NodeTypesWithDBAdapter;
use reth_primitives_traits::Recovered;
use reth_provider::{ProviderFactory, test_utils::create_test_provider_factory_with_node_types};
use reth_revm::{State, database::StateProviderDatabase};
use reth_transaction_pool::{
    BestTransactions, PoolTransaction, TransactionOrigin, ValidPoolTransaction,
    identifier::TransactionId, pool::PendingPool,
};

use crate::{
    BasePayloadBuilderCtx, FlashblockBlockDriver, FlashblockBlockOutcome,
    ParkableBestPayloadTransactions, RejectionCache,
};

type Ordering = BaseOrdering<BasePooledTransaction>;
type Pool = PendingPool<Ordering>;
type PooledTransaction = Arc<ValidPoolTransaction<BasePooledTransaction>>;
type GateProviderFactory =
    ProviderFactory<NodeTypesWithDBAdapter<BaseNode, Arc<TempDatabase<DatabaseEnv>>>>;

/// How the watched state of a workload's resting validity transactions is laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PredicateState {
    /// Every predicate of every transaction watches its own account.
    Unique,
    /// Predicates watch a small shared set of accounts, so many transactions park under the
    /// same predicate-index bucket.
    Shared {
        /// Number of shared watched accounts.
        accounts: usize,
    },
}

/// One block of pool traffic for the flashblock build gate. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlashblockWorkload {
    /// Stable scenario name used as the benchmark id and budget key.
    pub name: &'static str,
    /// Plain transfers that arrive before each flashblock. The flashblock gas target fits
    /// exactly these plus the satisfied validity transactions.
    pub transfers_per_flashblock: usize,
    /// Transfers beyond the gas target that also arrive before each flashblock, so every
    /// flashblock rejects a growing overflow for lack of gas.
    pub excess_transfers_per_flashblock: usize,
    /// Unsatisfiable validity transactions already in the pool when the block starts.
    pub resting_at_start: usize,
    /// Unsatisfiable validity transactions that arrive before each flashblock.
    pub resting_arrivals_per_flashblock: usize,
    /// Predicates on each unsatisfiable transaction. All but the last are satisfied, so every
    /// predicate is read before the last one blocks.
    pub predicates_per_resting_tx: usize,
    /// Watched-state layout of the unsatisfiable transactions.
    pub predicate_state: PredicateState,
    /// Whether transfers pay the shared watched accounts, so each committed transfer wakes a
    /// predicate-index bucket and the build loop rescans and re-parks its transactions.
    pub transfers_touch_watched_state: bool,
    /// Validity transactions whose predicates are satisfied, arriving before each flashblock.
    pub satisfied_validity_per_flashblock: usize,
}

impl FlashblockWorkload {
    /// Flashblocks per block: 2 s blocks at 200 ms flashblocks.
    pub const FLASHBLOCKS: u64 = 10;
    /// Gas used by every workload transaction (a plain transfer).
    pub const TRANSFER_GAS: u64 = 21_000;
    /// Predicates per satisfied validity transaction.
    pub const SATISFIED_PREDICATES: usize = 4;
    /// Resting validity transactions in the backlog scenarios: thousands of candidates deferred
    /// on every flashblock, enough that per-candidate costs dominate the block.
    pub const RESTING_BACKLOG_SIZE: usize = 4_500;
    /// Chain id of the synthetic chain.
    const CHAIN_ID: u64 = 901;
    /// Balance seeded into every sender, far above any transfer's worst-case cost.
    const SENDER_BALANCE: u128 = 1_000_000_000_000_000_000_000_000;

    /// Plain transfers only: the baseline every other scenario is measured against.
    pub const TRANSFERS: Self = Self {
        name: "transfers",
        transfers_per_flashblock: 100,
        excess_transfers_per_flashblock: 0,
        resting_at_start: 0,
        resting_arrivals_per_flashblock: 0,
        predicates_per_resting_tx: 0,
        predicate_state: PredicateState::Unique,
        transfers_touch_watched_state: false,
        satisfied_validity_per_flashblock: 0,
    };

    /// A backlog of single-predicate validity transactions that stay unsatisfied and are
    /// re-considered, re-evaluated, and re-parked on every flashblock.
    pub const RESTING_BACKLOG: Self = Self {
        name: "resting_backlog",
        resting_at_start: Self::RESTING_BACKLOG_SIZE,
        predicates_per_resting_tx: 1,
        ..Self::TRANSFERS
    };

    /// The backlog with eight predicates per transaction on unique accounts, so each
    /// re-evaluation reads eight cold accounts.
    pub const RESTING_BACKLOG_MULTI_PREDICATE: Self = Self {
        name: "resting_backlog_multi_predicate",
        predicates_per_resting_tx: 8,
        ..Self::RESTING_BACKLOG
    };

    /// The backlog with eight predicates per transaction over sixteen shared
    /// accounts: warm reads and crowded predicate-index buckets.
    pub const RESTING_BACKLOG_SHARED_STATE: Self = Self {
        name: "resting_backlog_shared_state",
        predicates_per_resting_tx: 8,
        predicate_state: PredicateState::Shared { accounts: 16 },
        ..Self::RESTING_BACKLOG
    };

    /// Transfers that pay the watched accounts of a parked backlog, so every commit wakes a
    /// bucket and the loop rescans and re-parks its transactions within the flashblock.
    ///
    /// 125 accounts keep eight transactions per bucket, below
    /// `DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD`: flat buckets wake on any change to the
    /// watched balance, while ordered buckets wake only transactions the new value can satisfy.
    pub const WAKE_RESCAN: Self = Self {
        name: "wake_rescan",
        resting_at_start: 1_000,
        predicates_per_resting_tx: 1,
        predicate_state: PredicateState::Shared { accounts: 125 },
        transfers_touch_watched_state: true,
        ..Self::TRANSFERS
    };

    /// Pool churn: the backlog grows from 1,500 to 4,500 within the block as validity
    /// transactions arrive between flashblocks.
    pub const BACKLOG_GROWTH: Self = Self {
        name: "backlog_growth",
        resting_at_start: 1_500,
        resting_arrivals_per_flashblock: 300,
        predicates_per_resting_tx: 1,
        ..Self::TRANSFERS
    };

    /// More transfers arrive than fit: every flashblock rejects the overflow for lack of gas.
    pub const CONGESTED: Self =
        Self { name: "congested", excess_transfers_per_flashblock: 200, ..Self::TRANSFERS };

    /// Validity transactions whose predicates match and are included alongside transfers.
    pub const SATISFIED_VALIDITY: Self = Self {
        name: "satisfied_validity",
        satisfied_validity_per_flashblock: 50,
        ..Self::TRANSFERS
    };

    /// The gate's workload matrix, in budget-file order.
    pub const MATRIX: [Self; 8] = [
        Self::TRANSFERS,
        Self::RESTING_BACKLOG,
        Self::RESTING_BACKLOG_MULTI_PREDICATE,
        Self::RESTING_BACKLOG_SHARED_STATE,
        Self::WAKE_RESCAN,
        Self::BACKLOG_GROWTH,
        Self::CONGESTED,
        Self::SATISFIED_VALIDITY,
    ];

    /// Returns the matrix workload with `name`.
    pub fn by_name(name: &str) -> Option<Self> {
        Self::MATRIX.into_iter().find(|workload| workload.name == name)
    }

    /// Gas each flashblock adds to the block's cumulative gas target.
    pub const fn gas_per_flashblock(&self) -> u64 {
        (self.transfers_per_flashblock + self.satisfied_validity_per_flashblock) as u64
            * Self::TRANSFER_GAS
    }

    /// Gas target of the whole block: every flashblock's target, and the native builder's
    /// block gas limit.
    pub const fn block_gas_target(&self) -> u64 {
        self.gas_per_flashblock() * Self::FLASHBLOCKS
    }

    /// Transactions the block includes when the build loop behaves correctly.
    pub const fn expected_included(&self) -> u64 {
        (self.transfers_per_flashblock + self.satisfied_validity_per_flashblock) as u64
            * Self::FLASHBLOCKS
    }

    /// Unsatisfiable validity transactions in the pool while building flashblock `index`.
    pub const fn resting_at_flashblock(&self, index: u64) -> usize {
        self.resting_at_start + self.resting_arrivals_per_flashblock * (index as usize + 1)
    }

    /// Distinct unsatisfiable validity transactions seen over the block.
    pub const fn resting_total(&self) -> usize {
        self.resting_at_flashblock(Self::FLASHBLOCKS - 1)
    }

    /// Installs the process-global transaction event writer, appending JSONL to `path`, the way
    /// a production builder with transaction events enabled runs: events are serialized on the
    /// builder thread and handed to the background writer thread.
    ///
    /// The queue is sized so no block in the matrix can fill it. The production queue is lossy,
    /// and a run that dropped events because the writer thread fell behind would measure less
    /// work than one that did not.
    pub fn install_file_event_writer(path: &Path) -> eyre::Result<()> {
        GlobalTransactionEventWriter::init(Some(TransactionEventWriterConfig {
            enabled: true,
            required: true,
            queue_capacity: 1 << 20,
            ..TransactionEventWriterConfig::disabled(
                TransactionEventProducer::BaseBuilder,
                "builder-gate",
                path,
            )
        }))?;
        Ok(())
    }
}

/// Result of one native payload build of a [`FlashblockWorkload`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeBlockOutcome {
    /// Transactions in the built block.
    pub included: u64,
    /// State root of the built block.
    pub state_root: B256,
}

/// A [`FlashblockWorkload`] materialized into a pool, arrivals, and seeded state.
#[derive(Debug)]
pub struct FlashblockWorkloadFixture {
    /// The workload this fixture materializes.
    pub workload: FlashblockWorkload,
    pool: Pool,
    arrivals: Vec<Vec<PooledTransaction>>,
    chain_spec: Arc<BaseChainSpec>,
    provider_factory: GateProviderFactory,
}

impl FlashblockWorkloadFixture {
    /// Builds the pool, per-flashblock arrivals, and an MDBX database whose genesis funds every
    /// sender.
    pub fn new(workload: FlashblockWorkload) -> Self {
        Self::with_denim(workload, false)
    }

    /// Builds the fixture for [`Self::run_native_block`]: every arrival is already in the pool,
    /// and Denim is active at genesis with the `BaseTime` proxy it requires.
    pub fn new_native(workload: FlashblockWorkload) -> Self {
        let mut fixture = Self::with_denim(workload, true);
        for transaction in fixture.arrivals.drain(..).flatten() {
            fixture.pool.add_transaction(transaction, 0);
        }
        fixture
    }

    fn with_denim(workload: FlashblockWorkload, denim: bool) -> Self {
        let mut builder = FlashblockWorkloadBuilder::new(workload);
        let mut pool = PendingPool::new(Ordering::coinbase_tip());
        for _ in 0..workload.resting_at_start {
            pool.add_transaction(builder.resting(), 0);
        }
        let arrivals = (0..FlashblockWorkload::FLASHBLOCKS)
            .map(|_| {
                let mut arrivals = Vec::new();
                for _ in 0..workload.resting_arrivals_per_flashblock {
                    arrivals.push(builder.resting());
                }
                for _ in 0..workload.satisfied_validity_per_flashblock {
                    arrivals.push(builder.satisfied());
                }
                for _ in
                    0..workload.transfers_per_flashblock + workload.excess_transfers_per_flashblock
                {
                    arrivals.push(builder.transfer());
                }
                arrivals
            })
            .collect();
        let chain_spec = builder.chain_spec(denim);
        let provider_factory =
            create_test_provider_factory_with_node_types::<BaseNode>(Arc::clone(&chain_spec));
        init_genesis(&provider_factory).expect("genesis initializes");
        Self { workload, pool, arrivals, chain_spec, provider_factory }
    }

    /// Builds one block of [`FlashblockWorkload::FLASHBLOCKS`] flashblocks through
    /// [`FlashblockBlockDriver`], adding each flashblock's arrivals to the pool first.
    ///
    /// Arrivals are consumed, so each fixture builds one block. Taking `&mut self` keeps the
    /// database teardown out of a caller's measured region.
    pub fn run_block(&mut self) -> eyre::Result<FlashblockBlockOutcome> {
        let mut ctx = self.builder_context();
        let provider = self.provider_factory.latest()?;
        let mut state = State::builder()
            .with_database(StateProviderDatabase::new(provider))
            .with_bundle_update()
            .build();
        let driver = FlashblockBlockDriver {
            flashblocks: FlashblockWorkload::FLASHBLOCKS,
            gas_per_flashblock: self.workload.gas_per_flashblock(),
        };
        let pool = &mut self.pool;
        let arrivals = &mut self.arrivals;
        let outcome = driver.run_block(
            &mut ctx,
            &mut state,
            RejectionCache::new(10_000, Duration::from_secs(60)),
            |flashblock_index| {
                for transaction in arrivals[flashblock_index as usize].drain(..) {
                    pool.add_transaction(transaction, 0);
                }
                let mut best = pool.best();
                best.no_updates();
                ParkableBestPayloadTransactions::new(Box::new(ParkedBestTransactions::new(
                    best,
                    Ordering::coinbase_tip(),
                    0,
                )))
            },
            |_: &[TxHash]| {},
        )?;
        Ok(outcome)
    }

    /// Builds one block through the native payload builder: one pass over the pool, as a
    /// Denim block is built, ending with the state root.
    ///
    /// The pool is read, not drained, so every call builds the same block. Taking `&mut self`
    /// matches [`Self::run_block`], and returning the fixture keeps the database teardown out of a
    /// caller's measured region.
    pub fn run_native_block(&mut self) -> eyre::Result<NativeBlockOutcome> {
        let ctx = self.native_builder_context();
        let provider = self.provider_factory.latest()?;
        let pool = &self.pool;
        let outcome = NativeBuilder::new(|_| {
            let mut best = pool.best();
            best.no_updates();
            ParkableBestPayloadTransactions::new(Box::new(ParkedBestTransactions::new(
                best,
                Ordering::coinbase_tip(),
                0,
            )))
        })
        .build(StateProviderDatabase::new(&provider), &provider, None, ctx)?;
        let BuildOutcomeKind::Freeze(payload) = outcome else {
            eyre::bail!("a Denim build must freeze its payload, got {outcome:?}");
        };
        let block = payload.block();
        Ok(NativeBlockOutcome {
            included: block.body().transactions.len() as u64,
            state_root: block.header().state_root,
        })
    }

    /// A native builder context on top of the genesis block, with the block gas limit set to
    /// the workload's gas target and the wall-clock predicate cutoff disabled.
    fn native_builder_context(&self) -> NativePayloadBuilderCtx<BaseEvmConfig, BaseChainSpec> {
        let parent = Arc::new(self.chain_spec.sealed_genesis_header());
        let payload_id = PayloadId::new([0; 8]);
        let attributes = BasePayloadBuilderAttributes::<BaseTransactionSigned> {
            payload_attributes: EthPayloadBuilderAttributes {
                id: payload_id,
                parent: parent.hash(),
                timestamp: parent.timestamp + 2,
                parent_beacon_block_root: Some(B256::ZERO),
                ..Default::default()
            },
            gas_limit: Some(self.workload.block_gas_target()),
            ..Default::default()
        };
        NativePayloadBuilderCtx {
            evm_config: BaseEvmConfig::<_, BasePrimitives>::base(Arc::clone(&self.chain_spec)),
            // See `builder_context`: the wall-clock cutoff would make counts nondeterministic.
            builder_config: BaseBuilderConfig {
                predicate_eval_hard_cutoff: Duration::MAX,
                ..Default::default()
            },
            chain_spec: Arc::clone(&self.chain_spec),
            config: PayloadConfig::new(parent, attributes, payload_id),
            cancel: Default::default(),
            best_payload: None,
        }
    }

    /// A builder context on top of the genesis block, with the wall-clock predicate cutoff
    /// disabled.
    fn builder_context(&self) -> BasePayloadBuilderCtx {
        let parent = Arc::new(self.chain_spec.sealed_genesis_header());
        let mut ctx = BasePayloadBuilderCtx::for_test(Arc::clone(&self.chain_spec), parent);
        // The production cutoff is wall-clock time; under Valgrind it would trip at an
        // arbitrary, run-dependent candidate and make instruction counts nondeterministic.
        ctx.builder_config.predicate_eval_hard_cutoff = Duration::MAX;
        ctx
    }
}

/// Allocates senders, watched accounts, and transactions for a [`FlashblockWorkloadFixture`].
#[derive(Debug)]
pub struct FlashblockWorkloadBuilder {
    workload: FlashblockWorkload,
    next_sender: usize,
    next_resting: usize,
    next_watched: usize,
    next_transfer: usize,
    funded: Vec<Address>,
}

impl FlashblockWorkloadBuilder {
    /// Sender accounts start here; watched accounts are allocated below it.
    const SENDER_OFFSET: usize = 1 << 32;
    /// Priority fee of validity transactions, above transfers so every flashblock reaches the
    /// whole backlog before it fills.
    const VALIDITY_PRIORITY_FEE: u128 = 10;
    /// Priority fee of plain transfers.
    const TRANSFER_PRIORITY_FEE: u128 = 1;

    /// Starts allocating senders and watched accounts for `workload`.
    pub const fn new(workload: FlashblockWorkload) -> Self {
        Self {
            workload,
            next_sender: 0,
            next_resting: 0,
            next_watched: 0,
            next_transfer: 0,
            funded: Vec::new(),
        }
    }

    fn address(index: usize) -> Address {
        Address::from_word(U256::from(index + 1).into())
    }

    fn watched(&mut self) -> Address {
        self.next_watched += 1;
        Self::address(self.next_watched)
    }

    /// A validity transaction whose last predicate can never be satisfied.
    fn resting(&mut self) -> PooledTransaction {
        let resting_index = self.next_resting;
        self.next_resting += 1;
        let count = self.workload.predicates_per_resting_tx;
        let predicates = (0..count)
            .map(|predicate_index| {
                let address = match self.workload.predicate_state {
                    PredicateState::Unique => self.watched(),
                    PredicateState::Shared { accounts } => {
                        Self::address((resting_index + predicate_index) % accounts)
                    }
                };
                let blocking = predicate_index + 1 == count;
                ValidityPredicate::Balance {
                    address,
                    op: ValidityOperator::Equal,
                    // Unfunded accounts hold zero, and no transfer ever funds one to U256::MAX.
                    value: if blocking { U256::MAX } else { U256::ZERO },
                }
            })
            .collect();
        self.transaction(Self::VALIDITY_PRIORITY_FEE, Address::repeat_byte(0xee), predicates)
    }

    /// A validity transaction whose predicates all hold (unfunded accounts with zero balance).
    fn satisfied(&mut self) -> PooledTransaction {
        let predicates = (0..FlashblockWorkload::SATISFIED_PREDICATES)
            .map(|_| ValidityPredicate::Balance {
                address: self.watched(),
                op: ValidityOperator::Equal,
                value: U256::ZERO,
            })
            .collect();
        self.transaction(Self::VALIDITY_PRIORITY_FEE, Address::repeat_byte(0xee), predicates)
    }

    /// A plain transfer, paying a shared watched account when the workload wakes the backlog.
    fn transfer(&mut self) -> PooledTransaction {
        let transfer_index = self.next_transfer;
        self.next_transfer += 1;
        let to = match (self.workload.transfers_touch_watched_state, self.workload.predicate_state)
        {
            (true, PredicateState::Shared { accounts }) => Self::address(transfer_index % accounts),
            _ => Address::repeat_byte(0xee),
        };
        self.transaction(Self::TRANSFER_PRIORITY_FEE, to, Vec::new())
    }

    fn transaction(
        &mut self,
        priority_fee: u128,
        to: Address,
        predicates: Vec<ValidityPredicate>,
    ) -> PooledTransaction {
        let sender_index = Self::SENDER_OFFSET + self.next_sender;
        self.next_sender += 1;
        let sender = Self::address(sender_index);
        self.funded.push(sender);
        let tx = TxEip1559 {
            chain_id: FlashblockWorkload::CHAIN_ID,
            nonce: 0,
            gas_limit: FlashblockWorkload::TRANSFER_GAS,
            max_fee_per_gas: 1_000_000_000_000,
            max_priority_fee_per_gas: priority_fee,
            to: TxKind::Call(to),
            // Transactions share a test signature, so a per-sender value keeps their hashes
            // distinct.
            value: U256::from(sender_index),
            access_list: Default::default(),
            input: Bytes::new(),
        };
        let envelope = BaseTxEnvelope::Eip1559(tx.into_signed(Signature::test_signature()));
        let encoded_length = envelope.encode_2718_len();
        let transaction = BasePooledTransaction::new(
            Recovered::new_unchecked(BaseTransactionSigned::from(envelope), sender),
            encoded_length,
        )
        .with_validity_predicates(predicates);
        debug_assert_eq!(transaction.sender(), sender);

        Arc::new(ValidPoolTransaction {
            transaction_id: TransactionId::new((sender_index as u64).into(), 0),
            transaction,
            propagate: true,
            timestamp: std::time::Instant::now(),
            origin: TransactionOrigin::External,
            authority_ids: None,
        })
    }

    /// A Cancun chain whose genesis funds every sender and whose gas limit never binds before
    /// the per-flashblock gas targets. With `denim`, Denim is active at genesis and the genesis
    /// holds the `BaseTime` proxy that Denim's pre-execution step links.
    fn chain_spec(&self, denim: bool) -> Arc<BaseChainSpec> {
        let genesis = Genesis {
            gas_limit: 1_000_000_000,
            config: serde_json::from_value(serde_json::json!({
                "chainId": FlashblockWorkload::CHAIN_ID
            }))
            .expect("valid chain config"),
            ..Default::default()
        }
        .extend_accounts(self.funded.iter().map(|sender| {
            (
                *sender,
                GenesisAccount::default()
                    .with_balance(U256::from(FlashblockWorkload::SENDER_BALANCE)),
            )
        }));
        let genesis = if denim {
            genesis.extend_accounts([(
                Predeploys::BASE_TIME,
                GenesisAccount::default().with_code(Some(BaseTime::proxy_bytecode())).with_storage(
                    Some(
                        [(
                            B256::from(BaseTime::ADMIN_SLOT),
                            B256::left_padding_from(Predeploys::PROXY_ADMIN.as_slice()),
                        )]
                        .into(),
                    ),
                ),
            )])
        } else {
            genesis
        };
        let chain_spec = ChainSpec::builder()
            .chain(FlashblockWorkload::CHAIN_ID.into())
            .genesis(genesis)
            .cancun_activated()
            .build();
        let mut chain_spec = BaseChainSpec::from(chain_spec);
        if denim {
            chain_spec.inner.hardforks.insert(BaseUpgrade::Denim, ForkCondition::Timestamp(0));
        }
        Arc::new(chain_spec)
    }
}
