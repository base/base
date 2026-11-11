//! Fixture for the native candidate-scan workload: EIP-1559 transfers from distinct funded
//! senders and a post-Jovian mainnet build context, shared by the wall-clock bench
//! (`native_candidate_scan.rs`) and the deterministic instruction-count gate
//! (`native_candidate_scan_iai.rs`) so both measure the same workload.

use std::{cell::Cell, collections::VecDeque, rc::Rc, sync::Arc};

use alloy_consensus::{Header, SignableTransaction, Transaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B64, B256, Signature, TxHash, TxKind, U256};
use base_common_chains::BaseUpgrade;
use base_common_consensus::{BasePrimitives, BaseTxEnvelope};
use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
use base_execution_evm::BaseEvmConfig;
use base_execution_payload_builder::{
    BasePayloadBuilderAttributes, ParkablePayloadTransactions, builder::BasePayloadBuilderCtx,
    config::BaseBuilderConfig, payload::EthPayloadBuilderAttributes,
};
use base_execution_txpool::BasePooledTransaction;
use reth_basic_payload_builder::PayloadConfig;
use reth_ethereum_forks::ForkCondition;
use reth_payload_builder::PayloadId;
use reth_payload_util::PayloadTransactions;
use reth_primitives_traits::{Account, SealedHeader, SignedTransaction};
use reth_revm::test_utils::StateProviderTest;
use reth_transaction_pool::{TransactionOrigin, ValidPoolTransaction, identifier::TransactionId};

/// Post-Azul mainnet timestamp; Denim and Everest are disabled in [`context`].
pub(crate) const TIMESTAMP: u64 = 1_790_000_000;
/// Parent base fee and Jovian minimum base fee, in wei.
pub(crate) const BASE_FEE: u64 = 1_000_000;
/// Gas limit of each candidate transfer.
pub(crate) const TX_GAS_LIMIT: u64 = 100_000;
/// Gas a plain transfer uses.
pub(crate) const TRANSFER_GAS: u64 = 21_000;

pub(crate) type Candidate = Arc<ValidPoolTransaction<BasePooledTransaction>>;

/// Yields candidates in order; parking and promotion are unused because no candidate carries
/// validity predicates.
pub(crate) struct Candidates {
    pub(crate) queued: VecDeque<Candidate>,
    /// Candidates yielded so far, readable after the scan consumes the iterator.
    pub(crate) yielded: Rc<Cell<usize>>,
}

impl PayloadTransactions for Candidates {
    type Transaction = Candidate;

    fn next(&mut self, _ctx: ()) -> Option<Self::Transaction> {
        let next = self.queued.pop_front();
        self.yielded.set(self.yielded.get() + usize::from(next.is_some()));
        next
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
pub(crate) fn candidate(index: u64, gas_limit: u64) -> Candidate {
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
    // Pool admission computes the hash and the DA-size estimate, so candidates reach the builder
    // with both cached.
    reth_transaction_pool::PoolTransaction::hash(&transaction);
    transaction.estimated_compressed_size();
    Arc::new(ValidPoolTransaction {
        transaction_id: TransactionId::new(0.into(), transaction.nonce()),
        transaction,
        propagate: true,
        timestamp: std::time::Instant::now(),
        origin: TransactionOrigin::External,
        authority_ids: None,
    })
}

pub(crate) fn context(block_gas_limit: u64) -> BasePayloadBuilderCtx<BaseEvmConfig, BaseChainSpec> {
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

pub(crate) fn funded_provider(candidates: &[Candidate]) -> StateProviderTest {
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
