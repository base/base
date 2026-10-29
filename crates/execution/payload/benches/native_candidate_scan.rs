//! End-to-end benchmark of the native payload builder's candidate loop,
//! `BasePayloadBuilderCtx::execute_best_transactions`.
//!
//! Each iteration builds fresh state, applies pre-execution changes and sequencer transactions,
//! then times only the candidate scan. Candidates are EIP-1559 transfers from distinct funded
//! senders, built at a post-Jovian mainnet timestamp so the DA-footprint limit is active.
//!
//! - `fits/N`: every candidate fits and executes.
//! - `saturated/N`: the block has room for about 10% of the candidates; the rest are rejected by
//!   the block-limits check. This is the full-block regime, where candidates far outnumber
//!   inclusions.
//! - `oversized/N`: every candidate's gas limit exceeds the block's, so all are rejected by the
//!   block-limits check without executing.
//!
//! ```sh
//! cargo bench -p base-execution-payload-builder --bench native_candidate_scan
//! ```

use std::{collections::VecDeque, hint::black_box, sync::Arc, time::Duration};

use alloy_consensus::{Header, SignableTransaction, Transaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B64, B256, Signature, TxHash, TxKind, U256};
use base_common_chains::BaseUpgrade;
use base_common_consensus::{BasePrimitives, BaseTxEnvelope, Predeploys};
use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
use base_execution_evm::BaseEvmConfig;
use base_execution_payload_builder::{
    BasePayloadBuilderAttributes, ParkablePayloadTransactions,
    builder::{BasePayloadBuilderCtx, ExecutionInfo},
    config::BaseBuilderConfig,
    payload::EthPayloadBuilderAttributes,
};
use base_execution_txpool::BasePooledTransaction;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use reth_basic_payload_builder::PayloadConfig;
use reth_ethereum_forks::ForkCondition;
use reth_evm::execute::BlockBuilder;
use reth_payload_builder::PayloadId;
use reth_payload_util::PayloadTransactions;
use reth_primitives_traits::{Account, SealedHeader, SignedTransaction};
use reth_revm::{database::StateProviderDatabase, db::State, test_utils::StateProviderTest};
use reth_storage_api::StateProvider;
use reth_transaction_pool::{TransactionOrigin, ValidPoolTransaction, identifier::TransactionId};

/// Post-Azul mainnet timestamp; Denim and Everest are disabled in [`context`].
const TIMESTAMP: u64 = 1_790_000_000;
/// Parent base fee and Jovian minimum base fee, in wei.
const BASE_FEE: u64 = 1_000_000;
/// Gas limit of each candidate transfer.
const TX_GAS_LIMIT: u64 = 100_000;
/// Gas a plain transfer uses.
const TRANSFER_GAS: u64 = 21_000;

type Candidate = Arc<ValidPoolTransaction<BasePooledTransaction>>;

/// Yields candidates in order; parking and promotion are unused because no candidate carries
/// validity predicates.
struct Candidates {
    queued: VecDeque<Candidate>,
}

impl PayloadTransactions for Candidates {
    type Transaction = Candidate;

    fn next(&mut self, _ctx: ()) -> Option<Self::Transaction> {
        self.queued.pop_front()
    }

    fn mark_invalid(&mut self, _sender: Address, _nonce: u64) {}
}

impl ParkablePayloadTransactions for Candidates {
    type Pooled = BasePooledTransaction;

    fn park_current(&mut self) {}

    fn mark_current_committed(&mut self) {}

    fn promote(&mut self, _transaction_hash: TxHash) -> bool {
        false
    }

    fn discard_parked(&mut self, _transaction_hash: TxHash) -> bool {
        false
    }
}

/// A transfer whose value makes the test signature recover to a distinct sender.
fn candidate(index: u64, gas_limit: u64) -> Candidate {
    let envelope = BaseTxEnvelope::Eip1559(
        TxEip1559 {
            chain_id: 8_453,
            nonce: 0,
            gas_limit,
            max_fee_per_gas: 2_000_000_000,
            max_priority_fee_per_gas: 1,
            to: TxKind::Call(Address::repeat_byte(0x11)),
            value: U256::from(index + 1),
            ..Default::default()
        }
        .into_signed(Signature::test_signature()),
    );
    let encoded_len = envelope.encode_2718_len();
    let transaction: BasePooledTransaction = BasePooledTransaction::new(
        envelope.try_into_recovered().expect("test signature must recover"),
        encoded_len,
    );
    Arc::new(ValidPoolTransaction {
        transaction_id: TransactionId::new(0.into(), transaction.nonce()),
        transaction,
        propagate: true,
        timestamp: std::time::Instant::now(),
        origin: TransactionOrigin::External,
        authority_ids: None,
    })
}

fn context(block_gas_limit: u64) -> BasePayloadBuilderCtx<BaseEvmConfig, BaseChainSpec> {
    let chain_spec = Arc::new(
        BaseChainSpecBuilder::base_mainnet()
            .with_fork(BaseUpgrade::Denim, ForkCondition::Never)
            .with_fork(BaseUpgrade::Everest, ForkCondition::Never)
            .build(),
    );
    let parent = Arc::new(SealedHeader::seal_slow(Header {
        gas_limit: block_gas_limit,
        timestamp: TIMESTAMP - 2,
        base_fee_per_gas: Some(BASE_FEE),
        ..Default::default()
    }));
    let payload_id = PayloadId::new([0; 8]);
    let attributes = BasePayloadBuilderAttributes::<BaseTxEnvelope> {
        payload_attributes: EthPayloadBuilderAttributes {
            id: payload_id,
            parent: parent.hash(),
            timestamp: TIMESTAMP,
            parent_beacon_block_root: Some(B256::ZERO),
            ..Default::default()
        },
        gas_limit: Some(block_gas_limit),
        eip_1559_params: Some(B64::ZERO),
        min_base_fee: Some(BASE_FEE),
        ..Default::default()
    };
    BasePayloadBuilderCtx {
        evm_config: BaseEvmConfig::<_, BasePrimitives>::base(Arc::clone(&chain_spec)),
        builder_config: BaseBuilderConfig::default(),
        chain_spec,
        config: PayloadConfig::new(parent, attributes, payload_id),
        cancel: Default::default(),
        best_payload: None,
    }
}

fn funded_provider(candidates: &[Candidate]) -> StateProviderTest {
    let mut provider = StateProviderTest::default();
    for candidate in candidates {
        provider.insert_account(
            candidate.sender(),
            Account { balance: U256::MAX, ..Default::default() },
            None,
            Default::default(),
        );
    }
    provider
}

/// Times one candidate scan over fresh state, returning its duration and the number of
/// transfers included.
fn scan(
    ctx: &BasePayloadBuilderCtx<BaseEvmConfig, BaseChainSpec>,
    provider: &StateProviderTest,
    candidates: &[Candidate],
) -> (Duration, u64) {
    let mut db = State::builder()
        .with_database(StateProviderDatabase::new(provider.into_evm_state_provider()))
        .with_bundle_update()
        .build();
    db.load_cache_account(Predeploys::L1_BLOCK_INFO).expect("L1 block info must load");
    let mut builder = ctx.block_builder(&mut db).expect("block builder");
    builder.apply_pre_execution_changes().expect("pre-execution changes");
    let mut info: ExecutionInfo = ctx.execute_sequencer_transactions(&mut builder).expect("seq");
    let transactions = Candidates { queued: candidates.iter().cloned().collect() };

    let start = std::time::Instant::now();
    let cancelled =
        ctx.execute_best_transactions(&mut info, &mut builder, transactions, None).expect("scan");
    let elapsed = start.elapsed();
    black_box(cancelled);
    (elapsed, info.cumulative_gas_used / TRANSFER_GAS)
}

fn bench_native_candidate_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("native_candidate_scan");
    for count in [100u64, 1_000] {
        // (block gas limit, candidate gas limit, transfers included).
        // Room for every candidate's gas-limit reservation.
        let fits = (count * TX_GAS_LIMIT, TX_GAS_LIMIT, count);
        // Room for the reservation of one candidate past 10% of the candidates' executed gas.
        let tenth = count / 10;
        let saturated = (TX_GAS_LIMIT + tenth * TRANSFER_GAS, TX_GAS_LIMIT, tenth + 1);
        // Every candidate reserves more gas than the block holds.
        let oversized = (1_000_000, 2_000_000, 0);
        for (name, (block_gas_limit, tx_gas_limit, included)) in
            [("fits", fits), ("saturated", saturated), ("oversized", oversized)]
        {
            let candidates: Vec<_> = (0..count).map(|i| candidate(i, tx_gas_limit)).collect();
            let provider = funded_provider(&candidates);
            let ctx = context(block_gas_limit);
            assert_eq!(scan(&ctx, &provider, &candidates).1, included, "{name}/{count}");
            group.bench_with_input(BenchmarkId::new(name, count), &candidates, |b, candidates| {
                b.iter_custom(|iters| {
                    (0..iters).map(|_| scan(&ctx, &provider, candidates).0).sum::<Duration>()
                });
            });
        }
    }
    group.finish();
}

criterion_group!(benches, bench_native_candidate_scan);
criterion_main!(benches);
