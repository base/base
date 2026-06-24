//! `eth_estimateGas` gas estimation for EIP-8130 simulation requests.

use alloy_eips::BlockId;
use alloy_evm::{
    EvmFactory,
    overrides::{apply_block_overrides, apply_state_overrides},
};
use alloy_primitives::U256;
use alloy_rpc_types::state::EvmOverrides;
use base_common_evm::BaseTransaction as BaseRevm;
use base_common_network::Base;
use base_common_rpc_types::BaseTransactionRequest;
use jsonrpsee_types::{ErrorObjectOwned, error::INVALID_PARAMS_CODE};
use reth_evm::{EvmFactoryFor, TxEnvFor};
use reth_rpc_eth_api::{
    FromEthApiError,
    helpers::{FullEthApi, LoadPendingBlock},
};
use revm::context::{Block, BlockEnv, TxEnv};

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
///
/// **Revert semantics differ from standard `eth_estimateGas`.** The standard
/// estimator returns an execution error (carrying revert data) when the call
/// reverts. An EIP-8130 estimate instead returns the gas charge even when one or
/// more phases revert: an EIP-8130 transaction is still *included* on-chain
/// (nonce consumed, fee paid) when its phases revert, so the charged gas is
/// exactly what the sender must fund. Surfacing it — rather than erroring — lets
/// callers set a correct gas limit for the always-included transaction.
#[derive(Debug)]
pub struct Eip8130GasEstimator;

impl Eip8130GasEstimator {
    /// Resolves the EVM environment at `block_id`, builds the unsigned
    /// simulation transaction, applies any `overrides` (block then state, to
    /// match the standard call path), and runs the EIP-8130 simulation,
    /// returning the gas it would charge.
    ///
    /// Block overrides are threaded through (not just state overrides) so the
    /// simulation runs against the same block env — basefee, timestamp, etc. —
    /// as the standard `eth_estimateGas` path.
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
        overrides: EvmOverrides,
    ) -> Result<U256, ErrorObjectOwned>
    where
        Eth: FullEthApi<NetworkTypes = Base> + LoadPendingBlock + Clone + Send + Sync + 'static,
        Eth::Error: FromEthApiError,
        TxEnvFor<Eth::Evm>: From<BaseRevm<TxEnv>>,
        // Pin the block env to revm's concrete type so block overrides can be
        // applied directly (Base's `EvmFactory::BlockEnv` is `revm::BlockEnv`).
        EvmFactoryFor<Eth::Evm>: EvmFactory<BlockEnv = BlockEnv>,
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

        let EvmOverrides { state, block } = overrides;

        let result = eth_api
            .spawn_with_state_at_block(at, move |this, mut db| {
                let mut evm_env = evm_env;
                // Block overrides first (mutating the block env), then state, so
                // the simulation matches the standard call path's ordering.
                if let Some(block) = block {
                    apply_block_overrides(*block, &mut db, &mut evm_env.block_env);
                }
                if let Some(state) = state {
                    apply_state_overrides(state, &mut db).map_err(Eth::Error::from_eth_err)?;
                }
                this.transact(db, evm_env, sim_tx.into())
            })
            .await?;

        // Return the charged gas even on revert — see the type-level note: an
        // EIP-8130 transaction is included (and pays) regardless of phase
        // reverts, so this is the gas the sender must fund.
        Ok(U256::from(result.result.tx_gas_used()))
    }
}
