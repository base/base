//! Fixture for the native build-loop workload: a pool of never-satisfied resting validity
//! transactions plus one block of filler transfers, and a post-Denim payload context, shared by
//! the wall-clock bench (`native_build_loop.rs`) and the deterministic instruction-count gate
//! (`native_build_loop_iai.rs`) so both measure the same workload.

use std::{sync::Arc, time::Instant};

use alloy_consensus::{Header, SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, Signature, TxKind, U256, keccak256};
use base_common_chains::BaseUpgrade;
use base_common_consensus::{BaseTransactionSigned, BaseTxEnvelope};
use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
use base_execution_evm::BaseEvmConfig;
use base_execution_payload_builder::{
    BasePayloadBuilderAttributes, ParkableBestPayloadTransactions, REJECTION_CACHE_MAX_CAPACITY,
    REJECTION_CACHE_TTL, RejectionCache, RestingPredicateMode, builder::BasePayloadBuilderCtx,
    config::BaseBuilderConfig, payload::EthPayloadBuilderAttributes,
};
use base_execution_txpool::{
    BaseOrdering, BasePooledTransaction, ParkedBestTransactions, ValidityOperator,
    ValidityPredicate,
};
use reth_basic_payload_builder::PayloadConfig;
use reth_ethereum_forks::ForkCondition;
use reth_payload_builder::PayloadId;
use reth_primitives_traits::{Recovered, SealedHeader};
use reth_transaction_pool::{
    TransactionOrigin, ValidPoolTransaction, identifier::TransactionId, pool::PendingPool,
};
use revm::{database::InMemoryDB, state::AccountInfo};

type Ordering = BaseOrdering<BasePooledTransaction>;
pub(crate) type Pool = PendingPool<Ordering>;
pub(crate) type Ctx = BasePayloadBuilderCtx<BaseEvmConfig, BaseChainSpec>;

const CHAIN_ID: u64 = 8_453;
/// Plain transfers that fill each block.
pub(crate) const FILLER_PER_BLOCK: usize = 120;
const TRANSFER_GAS: u64 = 21_000;
/// Sender indexes start here so they do not collide with the watched predicate addresses.
const SENDER_OFFSET: usize = 1_000_000;
const DENIM_TIMESTAMP: u64 = 1;

/// A hash-derived address, so hash maps see keys distributed like real addresses whatever the
/// process's random hash seed; sequential small keys cluster under some seeds.
fn address(index: usize) -> Address {
    Address::from_word(keccak256(index.to_be_bytes()))
}

/// Builds an executable transfer. Resting transactions get a higher tip so every block
/// considers them before the filler.
fn transaction(
    sender_index: usize,
    priority_fee: u128,
    predicates: Vec<ValidityPredicate>,
) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
    let tx = TxEip1559 {
        chain_id: CHAIN_ID,
        nonce: 0,
        gas_limit: TRANSFER_GAS,
        max_fee_per_gas: 1_000_000_000_000,
        max_priority_fee_per_gas: priority_fee,
        to: TxKind::Call(Address::repeat_byte(0xee)),
        // Distinguishes otherwise identical transfers from different senders.
        value: U256::from(sender_index),
        access_list: Default::default(),
        input: Bytes::new(),
    };
    let envelope = BaseTxEnvelope::Eip1559(tx.into_signed(Signature::test_signature()));
    let encoded_length = envelope.encode_2718_len();
    let sender = address(SENDER_OFFSET + sender_index);
    let transaction = BasePooledTransaction::new(
        Recovered::new_unchecked(BaseTransactionSigned::from(envelope), sender),
        encoded_length,
    )
    .with_validity_predicates(predicates);
    // Pool admission computes the hash and the DA-size estimate, so transactions reach the
    // builder with both cached.
    reth_transaction_pool::PoolTransaction::hash(&transaction);
    transaction.estimated_compressed_size();

    Arc::new(ValidPoolTransaction {
        transaction_id: TransactionId::new(((SENDER_OFFSET + sender_index) as u64).into(), 0),
        transaction,
        propagate: true,
        timestamp: Instant::now(),
        origin: TransactionOrigin::External,
        authority_ids: None,
    })
}

/// A pool of `resting` never-satisfied validity transactions, each watching its own balance, plus
/// one block of filler, and the parent state that funds every sender.
pub(crate) fn resting_pool(resting: usize) -> (Pool, InMemoryDB) {
    let mut pool = PendingPool::new(Ordering::coinbase_tip());
    let mut db = InMemoryDB::default();
    for sender_index in 0..resting + FILLER_PER_BLOCK {
        let (priority_fee, predicates) = if sender_index < resting {
            (
                10,
                vec![ValidityPredicate::Balance {
                    address: address(sender_index),
                    op: ValidityOperator::Equal,
                    value: U256::ONE,
                }],
            )
        } else {
            (1, Vec::new())
        };
        pool.add_transaction(transaction(sender_index, priority_fee, predicates), 0);
        db.insert_account_info(
            address(SENDER_OFFSET + sender_index),
            AccountInfo { balance: U256::from(u128::MAX), ..Default::default() },
        );
    }
    (pool, db)
}

pub(crate) fn parkable(pool: &Pool) -> ParkableBestPayloadTransactions<BasePooledTransaction> {
    ParkableBestPayloadTransactions::new(Box::new(ParkedBestTransactions::new(
        pool.best(),
        Ordering::coinbase_tip(),
        0,
    )))
}

/// A rejection cache with the builder's default sizing, holding `rejected` hashes of transactions
/// that are not in the pool, as a running builder accumulates from earlier blocks.
pub(crate) fn populated_rejection_cache(rejected: usize) -> RejectionCache {
    let cache = RejectionCache::new(REJECTION_CACHE_MAX_CAPACITY, REJECTION_CACHE_TTL);
    for index in 0..rejected {
        cache.insert(B256::from(U256::from(index) | (U256::from(1) << 255)));
    }
    cache.run_pending_tasks();
    cache
}

/// A Denim payload context whose gas limit fits exactly one block of filler.
pub(crate) fn payload_context(mode: RestingPredicateMode, rejection_cache: RejectionCache) -> Ctx {
    let chain_spec = Arc::new(
        BaseChainSpecBuilder::base_mainnet()
            .with_fork(BaseUpgrade::Denim, ForkCondition::Timestamp(DENIM_TIMESTAMP))
            .build(),
    );
    let gas_limit = FILLER_PER_BLOCK as u64 * TRANSFER_GAS;
    let parent = Arc::new(SealedHeader::seal_slow(Header { gas_limit, ..Default::default() }));
    let payload_id = PayloadId::new([0; 8]);
    let attributes = BasePayloadBuilderAttributes::<BaseTxEnvelope> {
        payload_attributes: EthPayloadBuilderAttributes {
            id: payload_id,
            parent: parent.hash(),
            timestamp: DENIM_TIMESTAMP,
            parent_beacon_block_root: Some(B256::ZERO),
            ..Default::default()
        },
        gas_limit: Some(gas_limit),
        ..Default::default()
    };
    BasePayloadBuilderCtx {
        evm_config: BaseEvmConfig::base(Arc::clone(&chain_spec)),
        builder_config: BaseBuilderConfig {
            rejection_cache,
            ..BaseBuilderConfig::default().with_resting_predicate_mode(mode)
        },
        chain_spec,
        config: PayloadConfig::new(parent, attributes, payload_id),
        cancel: Default::default(),
        best_payload: None,
    }
}
