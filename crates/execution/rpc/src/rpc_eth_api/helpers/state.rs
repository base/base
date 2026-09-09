//! Loads a pending block from database. Helper trait for `eth_` block, transaction, call and trace
//! RPC methods.

use std::{collections::HashMap, sync::Arc};

use alloy_eips::BlockId;
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_serde::JsonStorageKey;
use base_common_types_chain::constants::KECCAK_EMPTY;
use base_common_types_rpc::{
    Account, AccountInfo, BaseTransactionRequest, EIP1186AccountProofResponse,
};
use base_execution_evm::EvmEnvFor;
use base_execution_txpool::TransactionPool;
use futures::Future;
use reth_primitives_traits::RecoveredBlock;
use reth_rpc_eth_types::{
    BaseEthApiError, EthApiError, PendingBlockEnv, RpcInvalidTransactionError, SignError,
};
use reth_rpc_server_types::constants::DEFAULT_MAX_STORAGE_VALUES_SLOTS;
use reth_storage_api::{BlockIdReader, BlockReaderIdExt, StateProviderBox, StateProviderFactory};
use reth_trie_common::MultiProofTargets;

use crate::BaseEthApi;

/// Helper methods for `eth_` methods relating to state (accounts).
impl BaseEthApi {
    /// Validates that the given block is within the configured proof window.
    ///
    /// Returns an error if the distance between the chain tip and the requested block exceeds
    /// [`Self::max_proof_window`].
    pub fn ensure_within_proof_window(&self, block_id: BlockId) -> Result<(), BaseEthApiError> {
        let chain_info = self.chain_info().map_err(BaseEthApiError::from_eth_err)?;
        let block_number = self
            .provider()
            .block_number_for_id(block_id)
            .map_err(BaseEthApiError::from_eth_err)?
            .ok_or(EthApiError::HeaderNotFound(block_id))?;
        if chain_info.best_number.saturating_sub(block_number) > self.max_proof_window() {
            return Err(EthApiError::ExceedsMaxProofWindow.into());
        }
        Ok(())
    }

    /// Returns balance of given account, at given blocknumber.
    pub fn balance(
        &self,
        address: Address,
        block_id: Option<BlockId>,
    ) -> impl Future<Output = Result<U256, BaseEthApiError>> + Send {
        self.spawn_blocking_io_fut(async move |this| {
            Ok(this
                .state_at_block_id_or_latest(block_id)
                .await?
                .account_balance(&address)
                .map_err(BaseEthApiError::from_eth_err)?
                .unwrap_or_default())
        })
    }

    /// Returns values stored of given account, at given blocknumber.
    pub fn storage_at(
        &self,
        address: Address,
        index: JsonStorageKey,
        block_id: Option<BlockId>,
    ) -> impl Future<Output = Result<B256, BaseEthApiError>> + Send {
        self.spawn_blocking_io_fut(async move |this| {
            Ok(B256::new(
                this.state_at_block_id_or_latest(block_id)
                    .await?
                    .storage(address, index.as_b256())
                    .map_err(BaseEthApiError::from_eth_err)?
                    .unwrap_or_default()
                    .to_be_bytes(),
            ))
        })
    }

    /// Returns values from multiple storage positions across multiple addresses.
    ///
    /// Enforces a cap on total slot count (sum of all slot arrays) and returns an error if
    /// exceeded.
    pub fn storage_values(
        &self,
        requests: HashMap<Address, Vec<JsonStorageKey>>,
        block_id: Option<BlockId>,
    ) -> impl Future<Output = Result<HashMap<Address, Vec<B256>>, BaseEthApiError>> + Send {
        async move {
            if requests.is_empty() {
                return Err(BaseEthApiError::from_eth_err(EthApiError::InvalidParams(
                    "empty request".to_string(),
                )));
            }
            let total_slots: usize = requests.values().map(|slots| slots.len()).sum();
            if total_slots > DEFAULT_MAX_STORAGE_VALUES_SLOTS {
                return Err(BaseEthApiError::from_eth_err(EthApiError::InvalidParams(format!(
                    "total slot count {total_slots} exceeds limit {DEFAULT_MAX_STORAGE_VALUES_SLOTS}",
                ))));
            }

            self.spawn_blocking_io_fut(async move |this| {
                let state = this.state_at_block_id_or_latest(block_id).await?;

                let mut result = HashMap::with_capacity(requests.len());
                for (address, slots) in requests {
                    let mut values = Vec::with_capacity(slots.len());
                    for slot in &slots {
                        let value = state
                            .storage(address, slot.as_b256())
                            .map_err(BaseEthApiError::from_eth_err)?
                            .unwrap_or_default();
                        values.push(B256::new(value.to_be_bytes()));
                    }
                    result.insert(address, values);
                }

                Ok(result)
            })
            .await
        }
    }

    /// Returns values stored of given account, with Merkle-proof, at given blocknumber.
    pub fn get_proof(
        &self,
        address: Address,
        keys: Vec<JsonStorageKey>,
        block_id: Option<BlockId>,
    ) -> Result<
        impl Future<Output = Result<EIP1186AccountProofResponse, BaseEthApiError>> + Send,
        BaseEthApiError,
    > {
        Ok(async move {
            let _permit = self
                .acquire_owned_tracing()
                .await
                .map_err(|error| EthApiError::Internal(error.into()))?;

            let block_id = block_id.unwrap_or_default();
            self.ensure_within_proof_window(block_id)?;

            self.spawn_blocking_io_fut(async move |this| {
                let state = this.state_at_block_id(block_id).await?;
                let storage_keys = keys.iter().map(|key| key.as_b256()).collect::<Vec<_>>();
                let proof = state
                    .proof(Default::default(), address, &storage_keys)
                    .map_err(BaseEthApiError::from_eth_err)?;
                Ok(proof.into_eip1186_response(keys))
            })
            .await
        })
    }

    /// Returns account and storage proofs for multiple targets at the given block number.
    pub fn get_multi_proof(
        &self,
        targets: Vec<(Address, Vec<B256>)>,
        block_id: Option<BlockId>,
    ) -> Result<
        impl Future<Output = Result<Vec<EIP1186AccountProofResponse>, BaseEthApiError>> + Send,
        BaseEthApiError,
    > {
        Ok(async move {
            let _permit = self
                .acquire_owned_tracing()
                .await
                .map_err(|error| EthApiError::Internal(error.into()))?;

            let block_id = block_id.unwrap_or_default();
            self.ensure_within_proof_window(block_id)?;

            self.spawn_blocking_io_fut(async move |this| {
                let state = this.state_at_block_id(block_id).await?;
                let mut proof_targets = MultiProofTargets::with_capacity(targets.len());
                for (address, slots) in &targets {
                    proof_targets
                        .entry(keccak256(address))
                        .or_default()
                        .extend(slots.iter().map(keccak256));
                }

                let multiproof = state
                    .multiproof(Default::default(), proof_targets)
                    .map_err(BaseEthApiError::from_eth_err)?;

                targets
                    .into_iter()
                    .map(|(address, slots)| {
                        let proof = multiproof
                            .account_proof(address, &slots)
                            .map_err(|error| {
                                reth_rpc_eth_types::EthApiError::Internal(error.into())
                            })
                            .map_err(BaseEthApiError::from_eth_err)?;
                        let storage_keys =
                            slots.into_iter().map(JsonStorageKey::from).collect::<Vec<_>>();
                        Ok(proof.into_eip1186_response(storage_keys))
                    })
                    .collect::<Result<Vec<_>, BaseEthApiError>>()
            })
            .await
        })
    }

    /// Returns the account at the given address for the provided block identifier.
    pub fn get_account(
        &self,
        address: Address,
        block_id: BlockId,
    ) -> impl Future<Output = Result<Option<Account>, BaseEthApiError>> + Send {
        async move {
            self.ensure_within_proof_window(block_id)?;

            self.spawn_blocking_io_fut(async move |this| {
                let state = this.state_at_block_id(block_id).await?;
                let account =
                    state.basic_account(&address).map_err(BaseEthApiError::from_eth_err)?;
                let Some(account) = account else { return Ok(None) };

                let balance = account.balance;
                let nonce = account.nonce;
                let code_hash = account.bytecode_hash.unwrap_or(KECCAK_EMPTY);

                // Provide a default `HashedStorage` value in order to
                // get the storage root hash of the current state.
                let storage_root = state
                    .storage_root(address, Default::default())
                    .map_err(BaseEthApiError::from_eth_err)?;

                Ok(Some(Account { balance, nonce, code_hash, storage_root }))
            })
            .await
        }
    }

    /// Retrieves the account's balance, nonce, and code for a given address.
    pub fn get_account_info(
        &self,
        address: Address,
        block_id: BlockId,
    ) -> impl Future<Output = Result<AccountInfo, BaseEthApiError>> + Send {
        self.spawn_blocking_io_fut(async move |this| {
            let state = this.state_at_block_id(block_id).await?;
            let account = state
                .basic_account(&address)
                .map_err(BaseEthApiError::from_eth_err)?
                .unwrap_or_default();

            let balance = account.balance;
            let nonce = account.nonce;
            let code = if account.get_bytecode_hash() == KECCAK_EMPTY {
                Default::default()
            } else {
                state
                    .account_code(&address)
                    .map_err(BaseEthApiError::from_eth_err)?
                    .unwrap_or_default()
                    .original_bytes()
            };

            Ok(AccountInfo { balance, nonce, code })
        })
    }
}

/// Loads state from database.
///
/// Behaviour shared by several `eth_` RPC methods, not exclusive to `eth_` state RPC methods.
impl BaseEthApi {
    /// Returns the state at the given block number
    pub fn state_at_hash(&self, block_hash: B256) -> Result<StateProviderBox, BaseEthApiError> {
        self.provider().history_by_block_hash(block_hash).map_err(BaseEthApiError::from_eth_err)
    }

    /// Returns the state at the given [`BlockId`] enum.
    ///
    /// Note: if not [`BlockNumberOrTag::Pending`](alloy_eips::BlockNumberOrTag) then this
    /// will only return canonical state. See also <https://github.com/paradigmxyz/reth/issues/4515>
    pub fn state_at_block_id(
        &self,
        at: BlockId,
    ) -> impl Future<Output = Result<StateProviderBox, BaseEthApiError>> + Send {
        async move {
            if at.is_pending()
                && let Ok(Some(state)) = self.local_pending_state().await
            {
                return Ok(state);
            }

            self.provider().state_by_block_id(at).map_err(BaseEthApiError::from_eth_err)
        }
    }

    /// Returns the _latest_ state
    pub fn latest_state(&self) -> Result<StateProviderBox, BaseEthApiError> {
        self.provider().latest().map_err(BaseEthApiError::from_eth_err)
    }

    /// Returns the state at the given [`BlockId`] enum or the latest.
    ///
    /// Convenience function to interprets `None` as `BlockId::Number(BlockNumberOrTag::Latest)`
    pub fn state_at_block_id_or_latest(
        &self,
        block_id: Option<BlockId>,
    ) -> impl Future<Output = Result<StateProviderBox, BaseEthApiError>> + Send {
        async move {
            if let Some(block_id) = block_id {
                self.state_at_block_id(block_id).await
            } else {
                Ok(self.latest_state()?)
            }
        }
    }

    /// Returns the EVM environment for the given sealed header.
    pub fn evm_env_for_header(
        &self,
        header: &reth_primitives_traits::SealedHeader,
    ) -> Result<EvmEnvFor, BaseEthApiError> {
        self.evm_config()
            .evm_env(header)
            .map_err(|error| reth_rpc_eth_types::EthApiError::Internal(error.into()))
            .map_err(BaseEthApiError::from_eth_err)
    }

    /// Returns the EVM environment for the requested [`BlockId`]
    ///
    /// If the [`BlockId`] this will return the [`BlockId`] of the block the env was configured
    /// for.
    /// If the [`BlockId`] is pending, this will return the "Pending" tag, otherwise this returns
    /// the hash of the exact block.
    pub fn evm_env_at(
        &self,
        at: BlockId,
    ) -> impl Future<Output = Result<(EvmEnvFor, BlockId), BaseEthApiError>> + Send {
        async move {
            if at.is_pending() {
                let PendingBlockEnv { evm_env, origin } = self.pending_block_env_and_cfg()?;
                Ok((evm_env, origin.state_block_id()))
            } else {
                // we can assume that the blockid will be predominantly `Latest` (e.g. for
                // `eth_call`) and if requested by number or hash we can quickly fetch just the
                // header
                let header = self
                    .provider()
                    .sealed_header_by_id(at)
                    .map_err(BaseEthApiError::from_eth_err)?
                    .ok_or_else(|| EthApiError::HeaderNotFound(at))?;
                let evm_env = self.evm_env_for_header(&header)?;

                Ok((evm_env, header.hash().into()))
            }
        }
    }

    /// Returns the recovered block, EVM environment, and state block id for the requested
    /// [`BlockId`].
    ///
    /// For pending blocks, this preserves the state id returned by [`Self::evm_env_at`], which can
    /// be the pending tag for an actual pending block or the latest block hash when the pending env
    /// is derived from latest.
    #[expect(clippy::type_complexity)]
    pub fn evm_env_and_recovered_block_at(
        &self,
        at: BlockId,
    ) -> impl Future<Output = Result<(Arc<RecoveredBlock>, EvmEnvFor, BlockId), BaseEthApiError>> + Send
    {
        async move {
            if at.is_pending() {
                let (evm_env, block_id) = self.evm_env_at(at).await?;
                let block = self
                    .recovered_block(block_id)
                    .await?
                    .ok_or_else(|| EthApiError::HeaderNotFound(at))?;

                Ok((block, evm_env, block_id))
            } else {
                let block = self
                    .recovered_block(at)
                    .await?
                    .ok_or_else(|| EthApiError::HeaderNotFound(at))?;
                let evm_env = self.evm_env_for_header(block.sealed_block().sealed_header())?;
                let block_id = block.hash().into();

                Ok((block, evm_env, block_id))
            }
        }
    }

    /// Returns the next available nonce without gaps for the given address
    /// Next available nonce is either the on chain nonce of the account or the highest consecutive
    /// nonce in the pool + 1
    ///
    /// The provided request must have a from address set.
    pub fn next_available_nonce_for(
        &self,
        request: &BaseTransactionRequest,
    ) -> impl Future<Output = Result<u64, BaseEthApiError>> + Send {
        let address = request.as_ref().from;
        self.spawn_blocking_io(move |this| {
            let address = match address {
                Some(address) => address,
                None => return Err(SignError::NoAccount.into()),
            };

            // first fetch the on chain nonce of the account
            let mut next_nonce = this
                .latest_state()?
                .account_nonce(&address)
                .map_err(BaseEthApiError::from_eth_err)?
                .unwrap_or_default();

            // Retrieve the highest consecutive transaction for the sender from the transaction pool
            if let Some(highest_tx) =
                this.pool().get_highest_consecutive_transaction_by_sender(address, next_nonce)
            {
                // Return the nonce of the highest consecutive transaction + 1
                next_nonce = highest_tx.nonce().checked_add(1).ok_or_else(|| {
                    BaseEthApiError::from(EthApiError::InvalidTransaction(
                        RpcInvalidTransactionError::NonceMaxValue,
                    ))
                })?;
            }

            Ok(next_nonce)
        })
    }

    /// Returns the number of transactions sent from an address at the given block identifier.
    ///
    /// If this is [`BlockNumberOrTag::Pending`](alloy_eips::BlockNumberOrTag) then this will
    /// look up the highest transaction in pool and return the next nonce (highest + 1).
    pub fn transaction_count(
        &self,
        address: Address,
        block_id: Option<BlockId>,
    ) -> impl Future<Output = Result<U256, BaseEthApiError>> + Send {
        self.spawn_blocking_io_fut(async move |this| {
            // first fetch the on chain nonce of the account
            let on_chain_account_nonce = this
                .state_at_block_id_or_latest(block_id)
                .await?
                .account_nonce(&address)
                .map_err(BaseEthApiError::from_eth_err)?
                .unwrap_or_default();

            if block_id == Some(BlockId::pending()) {
                // for pending tag we need to find the highest nonce of txn in the pending state.
                if let Some(highest_pool_tx) = this
                    .pool()
                    .get_highest_consecutive_transaction_by_sender(address, on_chain_account_nonce)
                {
                    {
                        // and the corresponding txcount is nonce + 1 of the highest tx in the pool
                        // (on chain nonce is increased after tx)
                        let next_tx_nonce =
                            highest_pool_tx.nonce().checked_add(1).ok_or_else(|| {
                                BaseEthApiError::from(EthApiError::InvalidTransaction(
                                    RpcInvalidTransactionError::NonceMaxValue,
                                ))
                            })?;

                        // guard against drifts in the pool
                        let next_tx_nonce = on_chain_account_nonce.max(next_tx_nonce);

                        let tx_count = on_chain_account_nonce.max(next_tx_nonce);
                        return Ok(U256::from(tx_count));
                    }
                }
            }
            Ok(U256::from(on_chain_account_nonce))
        })
    }

    /// Returns code of given account, at the given identifier.
    pub fn get_code(
        &self,
        address: Address,
        block_id: Option<BlockId>,
    ) -> impl Future<Output = Result<Bytes, BaseEthApiError>> + Send {
        self.spawn_blocking_io_fut(async move |this| {
            Ok(this
                .state_at_block_id_or_latest(block_id)
                .await?
                .account_code(&address)
                .map_err(BaseEthApiError::from_eth_err)?
                .unwrap_or_default()
                .original_bytes())
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{
        Address, StorageKey, StorageValue, U256,
        map::{AddressMap, B256Map},
    };
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};

    use super::*;

    fn noop_eth_api() -> BaseEthApi {
        let provider = MockEthProvider::default();

        crate::test_utils::RpcTestUtils::api_builder(provider).build()
    }

    fn mock_eth_api(accounts: AddressMap<ExtendedAccount>) -> BaseEthApi {
        let mock_provider = MockEthProvider::default();

        mock_provider.extend_accounts(accounts);

        crate::test_utils::RpcTestUtils::api_builder(mock_provider).build()
    }

    #[tokio::test]
    async fn test_storage() {
        // === Noop ===
        let eth_api = noop_eth_api();
        let address = Address::random();
        let storage = eth_api.storage_at(address, U256::ZERO.into(), None).await.unwrap();
        assert_eq!(storage, U256::ZERO.to_be_bytes());

        // === Mock ===
        let storage_value = StorageValue::from(1337);
        let storage_key = StorageKey::random();
        let storage: B256Map<_> = core::iter::once((storage_key, storage_value)).collect();

        let accounts = AddressMap::from_iter([(
            address,
            ExtendedAccount::new(0, U256::ZERO).extend_storage(storage),
        )]);
        let eth_api = mock_eth_api(accounts);

        let storage_key: U256 = storage_key.into();
        let storage = eth_api.storage_at(address, storage_key.into(), None).await.unwrap();
        assert_eq!(storage, storage_value.to_be_bytes());
    }

    #[tokio::test]
    async fn test_get_account_missing() {
        let eth_api = noop_eth_api();
        let address = Address::random();
        let account = eth_api.get_account(address, Default::default()).await.unwrap();
        assert!(account.is_none());
    }
}
