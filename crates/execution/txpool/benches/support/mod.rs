//! Pool and transaction builders shared by the txpool benches.

use std::sync::Arc;

use alloy_consensus::{SignableTransaction, TxEip1559, transaction::SignerRecoverable};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_chains::ChainConfig;
use base_common_consensus::{BasePrimitives, BaseTxEnvelope};
use base_execution_chainspec::BaseChainSpec;
use base_execution_evm::BaseEvmConfig;
use base_execution_txpool::{
    BaseL1BlockInfo, BaseOrdering, BasePooledTransaction, BaseTransactionPool,
    BaseTransactionValidator, GuardLimits, ValidityOperator, ValidityPredicate,
};
use base_test_utils::build_test_genesis_everest;
use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
use reth_tasks::Runtime;
use reth_transaction_pool::{
    Pool, PoolConfig, SubPoolLimit, blobstore::InMemoryBlobStore,
    validate::EthTransactionValidatorBuilder,
};

pub(crate) type Client = MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>;
pub(crate) type BenchPool = BaseTransactionPool<Client, InMemoryBlobStore, BaseEvmConfig>;

const ONE_ETHER: u64 = 1_000_000_000_000_000_000;
const MIB: usize = 1024 * 1024;

const fn chain_id() -> u64 {
    ChainConfig::mainnet().chain_id
}

pub(crate) fn build_pool(queued_limit: usize) -> (BenchPool, Client) {
    let mut genesis = build_test_genesis_everest();
    genesis.config.chain_id = chain_id();
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(genesis));
    let client = MockEthProvider::<BasePrimitives>::new()
        .with_chain_spec(Arc::clone(&chain_spec))
        .with_genesis_block();
    let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));
    let blob_store = InMemoryBlobStore::default();
    let validator = EthTransactionValidatorBuilder::new(client.clone(), evm_config)
        .no_shanghai()
        .no_cancun()
        .build_with_tasks(Runtime::test(), blob_store.clone())
        .map(|inner| {
            let validator =
                BaseTransactionValidator::with_block_info(inner, BaseL1BlockInfo::default());
            let header = alloy_consensus::Header { timestamp: 1_700_000_000, ..Default::default() };
            validator.update_l1_block_info::<_, TxEip1559>(&header, &[]);
            validator
        });
    let config = PoolConfig {
        pending_limit: SubPoolLimit::new(100_000, 125 * MIB),
        queued_limit: SubPoolLimit::new(queued_limit, 25 * MIB),
        ..PoolConfig::default()
    };
    let ordering = BaseOrdering::default();
    let pool = Pool::new(validator, ordering.clone(), blob_store, config);
    (BaseTransactionPool::new(pool, ordering).with_guard_limits(GuardLimits::default()), client)
}

pub(crate) fn funded_signer(client: &Client) -> PrivateKeySigner {
    let signer = PrivateKeySigner::random();
    client.add_account(signer.address(), ExtendedAccount::new(0, U256::from(ONE_ETHER)));
    signer
}

pub(crate) fn standard(signer: &PrivateKeySigner, nonce: u64, tip: u128) -> BasePooledTransaction {
    let tx = TxEip1559 {
        chain_id: chain_id(),
        nonce,
        gas_limit: 50_000,
        max_fee_per_gas: 1_000,
        max_priority_fee_per_gas: tip,
        to: TxKind::Call(Address::repeat_byte(0xEE)),
        value: U256::ZERO,
        access_list: Default::default(),
        input: Bytes::new(),
    };
    let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
    let envelope = BaseTxEnvelope::Eip1559(tx.into_signed(signature));
    let recovered = envelope.clone().try_into_recovered().unwrap();
    BasePooledTransaction::new(recovered, envelope.encode_2718_len())
}

/// A validity transaction whose predicate holds for the next thousand blocks, so it rests in
/// the validity pool for the whole measurement.
pub(crate) fn validity(signer: &PrivateKeySigner, nonce: u64, tip: u128) -> BasePooledTransaction {
    standard(signer, nonce, tip).with_validity_predicates(vec![ValidityPredicate::BlockNumber {
        op: ValidityOperator::LessThanOrEqual,
        value: U256::from(1_000),
    }])
}
