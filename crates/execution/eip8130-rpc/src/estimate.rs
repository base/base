//! `eth_estimateGas` gas estimation for EIP-8130 simulation requests.

use alloy_eips::BlockId;
use alloy_evm::overrides::apply_state_overrides;
use alloy_primitives::U256;
use alloy_rpc_types::state::StateOverride;
use base_common_evm::BaseTransaction as BaseRevm;
use base_common_network::Base;
use base_common_rpc_types::BaseTransactionRequest;
use jsonrpsee_types::{ErrorObjectOwned, error::INVALID_PARAMS_CODE};
use reth_evm::TxEnvFor;
use reth_rpc_eth_api::{
    FromEthApiError,
    helpers::{FullEthApi, LoadPendingBlock},
};
use revm::context::{Block, TxEnv};

/// Estimates gas for an EIP-8130 `eth_estimateGas` request by running a single
/// read-only [`base_common_evm::Eip8130Executor::simulate`] at the block state.
///
/// Unlike the standard reth estimator, this does not binary-search a gas limit:
/// the EIP-8130 pipeline charges a deterministic, signature-independent amount
/// (intrinsic + phased-call gas, less the EIP-3529-capped refund, plus payer
/// authentication), so one simulation yields the exact estimate. The simulation
/// is built from an unsigned request with a stub authentication blob and never
/// commits state.
///
/// **Fork-agnostic on purpose.** This does not check Cobalt activation; callers
/// must gate via [`crate::Eip8130CobaltGate`] before invoking it.
#[derive(Debug)]
pub struct Eip8130GasEstimator;

impl Eip8130GasEstimator {
    /// Resolves the EVM environment at `block_id`, builds the unsigned
    /// simulation transaction, applies any `state_override`, and runs the
    /// EIP-8130 simulation, returning the gas it would charge.
    ///
    /// # Errors
    /// - `INVALID_PARAMS` if `request` carries no EIP-8130 fields (callers
    ///   should route plain requests to the standard estimator).
    /// - Any error from environment resolution, state access, override
    ///   application, or simulation propagates as an `ErrorObjectOwned`.
    pub async fn estimate<Eth>(
        eth_api: &Eth,
        request: BaseTransactionRequest,
        block_id: BlockId,
        state_override: Option<StateOverride>,
    ) -> Result<U256, ErrorObjectOwned>
    where
        Eth: FullEthApi<NetworkTypes = Base> + LoadPendingBlock + Clone + Send + Sync + 'static,
        Eth::Error: FromEthApiError,
        TxEnvFor<Eth::Evm>: From<BaseRevm<TxEnv>>,
        ErrorObjectOwned: From<Eth::Error>,
    {
        let (evm_env, at) = eth_api.evm_env_at(block_id).await?;
        let chain_id = evm_env.cfg_env.chain_id;
        // Bound execution by the block gas limit when the request omits `gas`.
        let gas_cap = Block::gas_limit(&evm_env.block_env);

        let sim_tx = request.to_eip8130_simulation_tx(chain_id, gas_cap).ok_or_else(|| {
            ErrorObjectOwned::owned(
                INVALID_PARAMS_CODE,
                "request carries no EIP-8130 fields or is missing the required `from` sender",
                None::<()>,
            )
        })?;

        let result = eth_api
            .spawn_with_state_at_block(at, move |this, mut db| {
                if let Some(overrides) = state_override {
                    apply_state_overrides(overrides, &mut db).map_err(Eth::Error::from_eth_err)?;
                }
                this.transact(db, evm_env, sim_tx.into())
            })
            .await?;

        Ok(U256::from(result.result.tx_gas_used()))
    }
}
