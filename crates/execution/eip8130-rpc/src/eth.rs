//! Standalone `eth_getTransactionCount` override that adds EIP-8130
//! `nonce_key` support on execution nodes.

use alloy_eips::BlockId;
use alloy_primitives::{Address, U256};
use base_common_types_rpc::BaseTransactionRequest;
use base_common_types_rpc::state::{EvmOverrides, StateOverride};
use base_evm_context::BlockEnv;
use base_execution_evm_blocks::{EvmFactoryFor, TxEnvFor};
use base_execution_evm_runtime::BaseTransaction as BaseRevm;
use base_execution_evm_runtime::EvmFactory;
use base_execution_rpc::BaseEthApi;
use jsonrpsee::{
    core::{RpcResult, async_trait},
    proc_macros::rpc,
};
use tracing::debug;

use crate::{ChannelNonceReader, Eip8130GasEstimator, Eip8130ZenithGate};

/// Eth API override trait that adds EIP-8130 `nonce_key` support to
/// `eth_getTransactionCount`.
///
/// Installed on every execution node.
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
    /// Uses the requested block state.
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
    /// `valid_after`/`valid_before`, or metadata) is estimated via a single
    /// read-only EIP-8130
    /// simulation against the block state (gated on the Zenith fork). The
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
pub struct Eip8130EthApiExt {
    eth_api: BaseEthApi,
}

impl Eip8130EthApiExt {
    /// Creates a new standalone EIP-8130 `eth_getTransactionCount`
    /// extension over the supplied BaseEthApi<ApiNode> API.
    pub const fn new(eth_api: BaseEthApi) -> Self {
        Self { eth_api }
    }
}

#[async_trait]
impl Eip8130EthApiOverrideServer for Eip8130EthApiExt
where
    TxEnvFor: From<BaseRevm>,
    EvmFactoryFor: EvmFactory<BlockEnv = BlockEnv>,
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
        // and falls through to the standard resolution. The Zenith gate
        // lives here (not above) so the default hot path — absent
        // `nonce_key` and `Some(0)` — is not slowed down by a sync header
        // resolution.
        if let Some(key) = nonce_key
            && key != U256::ZERO
        {
            Eip8130ZenithGate::check(&self.eth_api, block_id)?;
            return ChannelNonceReader::read(&self.eth_api, address, key, block_id, None).await;
        }

        // Protocol nonce path. Standard reth resolution against
        // `account.nonce` at the requested block.
        BaseEthApi::transaction_count(&self.eth_api, address, block_number)
            .await
            .map_err(Into::into)
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
            return BaseEthApi::estimate_gas_at(
                &self.eth_api,
                request,
                block_id,
                EvmOverrides::state(state_overrides),
            )
            .await
            .map_err(Into::into);
        }

        debug!(message = "rpc::eip8130::estimate_gas", block_id = ?block_id);

        Eip8130ZenithGate::check(&self.eth_api, block_id)?;
        // This standalone override only receives state overrides (the
        // `eth_estimateGas` RPC signature carries no block overrides); the
        Eip8130GasEstimator::estimate(
            &self.eth_api,
            request,
            block_id,
            EvmOverrides::state(state_overrides),
        )
        .await
    }
}
