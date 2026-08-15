//! Standalone `eth_getTransactionCount` override that adds EIP-8130
//! `nonce_key` support on nodes without flashblocks.

use alloy_eips::BlockId;
use alloy_evm::EvmFactory;
use alloy_primitives::{Address, U256};
use alloy_rpc_types::state::{EvmOverrides, StateOverride};
use base_common_chains::Upgrades;
use base_common_evm::BaseTransaction as BaseRevm;
use base_common_network::Base;
use base_common_rpc_types::BaseTransactionRequest;
use jsonrpsee::{
    core::{RpcResult, async_trait},
    proc_macros::rpc,
};
use reth_chainspec::ChainSpecProvider;
use reth_evm::{EvmFactoryFor, TxEnvFor};
use reth_rpc_eth_api::{
    EthApiTypes, FromEthApiError, RpcNodeCore,
    helpers::{EthCall, EthState, FullEthApi, LoadPendingBlock},
};
use reth_storage_api::BlockReaderIdExt;
use revm::context::{BlockEnv, TxEnv};
use tracing::debug;

use crate::{ChannelNonceReader, Eip8130CobaltGate, Eip8130GasEstimator};

/// Eth API override trait that adds EIP-8130 `nonce_key` support to
/// `eth_getTransactionCount`.
///
/// Registered only on nodes where the flashblocks override is not
/// registering, since flashblocks's override already extends the same
/// method with `nonce_key` plus its own pending-state semantics.
#[rpc(server, namespace = "eth")]
pub trait Eip8130EthApiOverride {
    /// Returns transaction count for an address.
    ///
    /// `nonce_key`: when omitted or zero, returns the protocol nonce from
    /// account state (the standard reth resolution). When non-zero,
    /// returns the 2D channel nonce `nonces[address][nonce_key]` from the
    /// Nonce Manager precompile. `nonce_key == NONCE_KEY_MAX` returns
    /// `INVALID_PARAMS`.
    ///
    /// No pending-flashblock state is consulted here; this override is for
    /// nodes running without flashblocks. Use the flashblocks override if
    /// pending-state semantics are required.
    #[method(name = "getTransactionCount")]
    async fn get_transaction_count(
        &self,
        address: Address,
        block_number: Option<BlockId>,
        nonce_key: Option<U256>,
    ) -> RpcResult<U256>;

    /// Estimates gas for a transaction.
    ///
    /// A request carrying EIP-8130 fields (account changes, calls, `nonce_key`,
    /// expiry, or metadata) is estimated via a single read-only EIP-8130
    /// simulation against the block state (gated on the Cobalt fork). The
    /// EIP-8130 pipeline charges deterministic, signature-independent gas, so no
    /// gas-limit binary search is needed. A plain request falls through to the
    /// standard reth estimator unchanged.
    #[method(name = "estimateGas")]
    async fn estimate_gas(
        &self,
        request: BaseTransactionRequest,
        block_number: Option<BlockId>,
        state_overrides: Option<StateOverride>,
    ) -> RpcResult<U256>;
}

/// Standalone EIP-8130 `eth_getTransactionCount` extension.
#[derive(Debug)]
pub struct Eip8130EthApiExt<Eth: EthApiTypes> {
    eth_api: Eth,
}

impl<Eth: EthApiTypes> Eip8130EthApiExt<Eth> {
    /// Creates a new standalone EIP-8130 `eth_getTransactionCount`
    /// extension over the supplied Eth API.
    pub const fn new(eth_api: Eth) -> Self {
        Self { eth_api }
    }
}

#[async_trait]
impl<Eth> Eip8130EthApiOverrideServer for Eip8130EthApiExt<Eth>
where
    Eth: FullEthApi<NetworkTypes = Base> + LoadPendingBlock + Clone + Send + Sync + 'static,
    Eth::Error: FromEthApiError,
    <Eth as RpcNodeCore>::Provider: ChainSpecProvider + BlockReaderIdExt,
    <<Eth as RpcNodeCore>::Provider as ChainSpecProvider>::ChainSpec: Upgrades,
    TxEnvFor<Eth::Evm>: From<BaseRevm<TxEnv>>,
    EvmFactoryFor<Eth::Evm>: EvmFactory<BlockEnv = BlockEnv>,
    jsonrpsee_types::error::ErrorObject<'static>: From<Eth::Error>,
{
    async fn get_transaction_count(
        &self,
        address: Address,
        block_number: Option<BlockId>,
        nonce_key: Option<U256>,
    ) -> RpcResult<U256> {
        debug!(
            message = "rpc::eip8130::get_transaction_count",
            address = %address,
            nonce_key = ?nonce_key,
        );

        let block_id = block_number.unwrap_or_default();

        // EIP-8130 channel read. Only `nonce_key != 0` uses the precompile
        // path; `Some(0)` is the protocol nonce by EIP-8130's reservation
        // and falls through to the standard resolution. The Cobalt gate
        // lives here (not above) so the default hot path — absent
        // `nonce_key` and `Some(0)` — is not slowed down by a sync header
        // resolution.
        if let Some(key) = nonce_key
            && key != U256::ZERO
        {
            Eip8130CobaltGate::check(&self.eth_api, block_id)?;
            return ChannelNonceReader::read(&self.eth_api, address, key, block_id, None).await;
        }

        // Protocol nonce path. Standard reth resolution against
        // `account.nonce` at the requested block. No flashblocks
        // pending-state delta. This override only registers when
        // flashblocks is disabled.
        EthState::transaction_count(&self.eth_api, address, block_number).await.map_err(Into::into)
    }

    async fn estimate_gas(
        &self,
        request: BaseTransactionRequest,
        block_number: Option<BlockId>,
        state_overrides: Option<StateOverride>,
    ) -> RpcResult<U256> {
        let block_id = block_number.unwrap_or_default();

        // Plain (non-8130) request: this override replaces the default
        // `eth_estimateGas`, so the common case must be delegated to the
        // standard reth estimator unchanged.
        if request.as_eip8130().is_none() {
            return EthCall::estimate_gas_at(
                &self.eth_api,
                request,
                block_id,
                EvmOverrides::state(state_overrides),
            )
            .await
            .map_err(Into::into);
        }

        debug!(message = "rpc::eip8130::estimate_gas", block_id = ?block_id);

        Eip8130CobaltGate::check(&self.eth_api, block_id)?;
        // This standalone override only receives state overrides (the
        // `eth_estimateGas` RPC signature carries no block overrides); the
        // estimator still accepts the full `EvmOverrides` so the flashblocks
        // path can thread its pending block env through.
        Eip8130GasEstimator::estimate(
            &self.eth_api,
            request,
            block_id,
            EvmOverrides::state(state_overrides),
        )
        .await
    }
}
