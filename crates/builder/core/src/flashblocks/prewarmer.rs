//! Speculative transaction execution for warming the shared execution cache.

use alloy_primitives::B256;
use base_common_consensus::BaseTransactionSigned;
use base_common_evm::BaseSpecId;
use base_execution_evm::BaseEvmConfig;
use reth_evm::{ConfigureEvm, Evm, EvmEnv};
use reth_execution_cache::{CachedStateProvider, ExecutionCache};
use reth_provider::StateProviderFactory;
use reth_revm::database::StateProviderDatabase;
use reth_transaction_pool::PoolTransaction;
use tracing::trace;

/// Context for speculatively executing transactions against parent state.
///
/// Each transaction receives a fresh state provider and EVM. State changes and execution results
/// are discarded; only reads populated through the shared [`ExecutionCache`] are retained.
#[derive(Clone, Debug)]
pub struct PrewarmingExecutionContext<Client> {
    client: Client,
    parent_hash: B256,
    evm_config: BaseEvmConfig,
    evm_env: EvmEnv<BaseSpecId>,
    execution_cache: ExecutionCache,
}

impl<Client> PrewarmingExecutionContext<Client>
where
    Client: StateProviderFactory,
{
    /// Creates a speculative execution context for one payload build.
    pub const fn new(
        client: Client,
        parent_hash: B256,
        evm_config: BaseEvmConfig,
        evm_env: EvmEnv<BaseSpecId>,
        execution_cache: ExecutionCache,
    ) -> Self {
        Self { client, parent_hash, evm_config, evm_env, execution_cache }
    }

    /// Executes `transaction` against parent state to populate the shared execution cache.
    ///
    /// Nonce and balance checks are disabled so transactions that depend on earlier block state can
    /// still warm useful reads. The execution result and all state changes are discarded.
    ///
    /// Returns `true` when speculative execution completed and `false` when the provider could not
    /// be opened or the EVM rejected execution. Failures are deliberately non-fatal because
    /// prewarming is only an optimization.
    #[must_use]
    pub fn prewarm_transaction<T>(&self, transaction: &T) -> bool
    where
        T: PoolTransaction<Consensus = BaseTransactionSigned>,
    {
        let state_provider = match self.client.state_by_block_hash(self.parent_hash) {
            Ok(provider) => provider,
            Err(error) => {
                trace!(
                    target: "payload_builder",
                    error = %error,
                    parent_hash = ?self.parent_hash,
                    transaction_hash = ?transaction.hash(),
                    "failed to open parent state for transaction prewarming",
                );
                return false;
            }
        };
        let state_provider =
            CachedStateProvider::new_prewarm(state_provider, self.execution_cache.clone());
        let database = StateProviderDatabase::new(state_provider);

        let mut evm_env = self.evm_env.clone();
        evm_env.cfg_env.disable_nonce_check = true;
        evm_env.cfg_env.disable_balance_check = true;
        let mut evm = self.evm_config.evm_with_env(database, evm_env);
        let transaction = transaction.clone_into_consensus();

        match evm.transact(&transaction) {
            Ok(_) => true,
            Err(error) => {
                trace!(
                    target: "payload_builder",
                    error = %error,
                    transaction_hash = ?transaction.tx_hash(),
                    "failed to speculatively execute transaction for prewarming",
                );
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::{Header, SignableTransaction, TxEip1559};
    use alloy_eips::{Encodable2718, eip1559::MIN_PROTOCOL_BASE_FEE};
    use alloy_primitives::{Address, B256, Bytes, TxKind, U256, hex, keccak256};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use base_common_consensus::{BaseTransactionSigned, BaseTypedTransaction};
    use base_execution_chainspec::BaseChainSpec;
    use base_execution_evm::{BaseEvmConfig, BaseNextBlockEnvAttributes};
    use base_execution_txpool::BasePooledTransaction;
    use reth_chainspec::ChainSpec;
    use reth_evm::ConfigureEvm;
    use reth_execution_cache::{CachedStatus, ExecutionCache, SavedCache};
    use reth_primitives_traits::Recovered;
    use reth_provider::{
        AccountReader,
        test_utils::{ExtendedAccount, MockEthProvider},
    };

    use super::PrewarmingExecutionContext;

    const CHAIN_ID: u64 = 901;
    const CONTRACT_STORAGE_VALUE: u64 = 42;

    type TestClient = MockEthProvider<base_common_consensus::BasePrimitives>;

    struct Fixture {
        context: PrewarmingExecutionContext<TestClient>,
        client: TestClient,
        cache: SavedCache,
        transaction: BasePooledTransaction,
        sender: Address,
        contract: Address,
        code_hash: B256,
    }

    impl Fixture {
        fn new(transaction_nonce: u64) -> Self {
            Self::with_transaction_chain_id(transaction_nonce, CHAIN_ID)
        }

        fn with_transaction_chain_id(transaction_nonce: u64, transaction_chain_id: u64) -> Self {
            let genesis: serde_json::Value = serde_json::json!({
                "config": { "chainId": CHAIN_ID },
                "gasLimit": "0x1C9C380",
                "timestamp": "0x0"
            });
            let genesis = serde_json::from_value(genesis).expect("valid genesis");
            let chain_spec = Arc::new(BaseChainSpec::from(
                ChainSpec::builder()
                    .chain(CHAIN_ID.into())
                    .genesis(genesis)
                    .cancun_activated()
                    .build(),
            ));
            let evm_config = BaseEvmConfig::base(chain_spec);
            let parent = Header {
                gas_limit: 30_000_000,
                base_fee_per_gas: Some(MIN_PROTOCOL_BASE_FEE),
                ..Default::default()
            };
            let evm_env = evm_config
                .next_evm_env(
                    &parent,
                    &BaseNextBlockEnvAttributes {
                        timestamp: 1,
                        suggested_fee_recipient: Address::random(),
                        prev_randao: B256::random(),
                        gas_limit: parent.gas_limit,
                        parent_beacon_block_root: None,
                        extra_data: Default::default(),
                    },
                )
                .expect("valid next block environment");

            let signer = PrivateKeySigner::random();
            let sender = signer.address();
            let contract = Address::random();
            let bytecode: Bytes = hex!("60005400").into();
            let code_hash = keccak256(&bytecode);
            let client = TestClient::new();
            client.add_account(
                contract,
                ExtendedAccount::new(0, U256::ZERO)
                    .with_bytecode(bytecode)
                    .extend_storage([(B256::ZERO, U256::from(CONTRACT_STORAGE_VALUE))]),
            );

            let transaction = TxEip1559 {
                chain_id: transaction_chain_id,
                nonce: transaction_nonce,
                gas_limit: 100_000,
                max_fee_per_gas: MIN_PROTOCOL_BASE_FEE as u128,
                max_priority_fee_per_gas: 0,
                to: TxKind::Call(contract),
                ..Default::default()
            };
            let signature =
                signer.sign_hash_sync(&transaction.signature_hash()).expect("sign transaction");
            let signed = BaseTransactionSigned::new_unhashed(
                BaseTypedTransaction::Eip1559(transaction),
                signature,
            );
            let recovered = Recovered::new_unchecked(signed, sender);
            let encoded_length = recovered.encode_2718_len();
            let transaction = BasePooledTransaction::new(recovered, encoded_length);

            let cache = SavedCache::new(B256::ZERO, ExecutionCache::new(1_000_000));
            let context = PrewarmingExecutionContext::new(
                client.clone(),
                B256::ZERO,
                evm_config,
                evm_env,
                cache.cache().clone(),
            );

            Self { context, client, cache, transaction, sender, contract, code_hash }
        }
    }

    #[test]
    fn speculative_execution_warms_account_code_and_storage_reads() {
        let fixture = Fixture::new(0);

        assert!(fixture.context.prewarm_transaction(&fixture.transaction));

        assert!(matches!(
            fixture
                .cache
                .cache()
                .get_or_try_insert_account_with(fixture.sender, || { Ok::<_, ()>(None) }),
            Ok(CachedStatus::Cached(None))
        ));
        assert!(matches!(
            fixture
                .cache
                .cache()
                .get_or_try_insert_account_with(fixture.contract, || { Ok::<_, ()>(None) }),
            Ok(CachedStatus::Cached(Some(_)))
        ));
        assert!(matches!(
            fixture
                .cache
                .cache()
                .get_or_try_insert_code_with(fixture.code_hash, || { Ok::<_, ()>(None) }),
            Ok(CachedStatus::Cached(Some(_)))
        ));
        assert_eq!(
            fixture.cache.cache().get_or_try_insert_storage_with(
                fixture.contract,
                B256::ZERO,
                || Ok::<_, ()>(U256::ZERO),
            ),
            Ok(CachedStatus::Cached(U256::from(CONTRACT_STORAGE_VALUE)))
        );
    }

    #[test]
    fn speculative_execution_ignores_nonce_and_balance_checks() {
        let fixture = Fixture::new(7);

        assert!(
            fixture.context.prewarm_transaction(&fixture.transaction),
            "an unfunded sender with a future nonce should still execute for cache warming"
        );
    }

    #[test]
    fn speculative_execution_reports_transaction_failure() {
        let fixture = Fixture::with_transaction_chain_id(0, CHAIN_ID + 1);

        assert!(
            !fixture.context.prewarm_transaction(&fixture.transaction),
            "a transaction invalid for the configured chain should not report successful execution"
        );
    }

    #[test]
    fn speculative_execution_does_not_commit_state_changes() {
        let fixture = Fixture::new(0);
        let original = fixture.client.basic_account(&fixture.sender).expect("read sender account");

        assert!(fixture.context.prewarm_transaction(&fixture.transaction));
        assert_eq!(
            fixture.client.basic_account(&fixture.sender).expect("read sender account"),
            original,
            "speculative sender nonce and balance changes must be discarded"
        );
    }

    #[test]
    fn dropping_context_releases_shared_cache_handle() {
        let fixture = Fixture::new(0);

        assert!(!fixture.cache.is_available(), "context must hold the shared cache");
        assert_eq!(
            fixture.cache.usage_count(),
            2,
            "only the saved cache and context should remain"
        );
        assert!(fixture.context.prewarm_transaction(&fixture.transaction));
        assert_eq!(
            fixture.cache.usage_count(),
            2,
            "the per-transaction provider must release its cache handle after execution"
        );
        drop(fixture.context);
        assert!(fixture.cache.is_available(), "dropping context must release the shared cache");
    }
}
