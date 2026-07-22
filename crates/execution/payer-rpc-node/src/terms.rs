//! State-backed [`PayerTerms`] resolver.

use std::time::{SystemTime, UNIX_EPOCH};

use alloy_primitives::{Address, B256, U256};
use base_execution_chainspec::BaseChainSpec;
use base_execution_payer::{PayerConfigStorage, PriceSnapshot};
use base_execution_payer_rpc::{PayerTerms, PayerTermsError};
use base_precompile_storage::{
    BasePrecompileError, ReadOnlyStorage, Result as StorageResult, StorageCtx, StorageReader,
};
use reth_provider::{ChainSpecProvider, StateProvider, StateProviderFactory};

/// A read-only [`StorageReader`] over a reth [`StateProvider`]: each read is a
/// single committed-state `SLOAD`.
struct StateProviderReader<'a>(&'a dyn StateProvider);

impl StorageReader for StateProviderReader<'_> {
    fn sload(&self, address: Address, key: U256) -> StorageResult<U256> {
        self.0
            .storage(address, B256::from(key.to_be_bytes()))
            .map_err(|error| BasePrecompileError::Fatal(error.to_string()))
            .map(|value| value.unwrap_or_default())
    }
}

/// Resolves ERC-8168 payer terms by decoding the on-chain payer config against
/// the node's latest committed state.
///
/// The resolver reads the config and each slot-backed token's price through a
/// read-only precompile-storage adapter — no EVM execution, just `SLOAD`s — and
/// enforces oracle staleness against the current wall-clock time.
#[derive(Debug, Clone)]
pub struct StateBackedPayerTerms<Provider> {
    provider: Provider,
}

impl<Provider> StateBackedPayerTerms<Provider> {
    /// Wraps the node's state provider.
    pub const fn new(provider: Provider) -> Self {
        Self { provider }
    }
}

impl<Provider> PayerTerms for StateBackedPayerTerms<Provider>
where
    Provider: StateProviderFactory + ChainSpecProvider<ChainSpec = BaseChainSpec> + Send + Sync,
{
    fn price_snapshot(&self) -> Result<PriceSnapshot, PayerTermsError> {
        let state = self.provider.latest().map_err(PayerTermsError::new)?;
        let chain_id = self.provider.chain_spec().chain().id();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or_default();

        let mut storage = ReadOnlyStorage::new(StateProviderReader(&*state), chain_id, now);
        StorageCtx::enter(&mut storage, |ctx| PayerConfigStorage::new(ctx).price_snapshot(now))
            .map_err(PayerTermsError::new)
    }
}
