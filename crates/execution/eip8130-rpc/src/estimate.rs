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
use reth_evm::{EvmFactoryFor, HaltReasonFor, TxEnvFor};
use reth_rpc_eth_api::{
    FromEthApiError,
    helpers::{FullEthApi, LoadPendingBlock},
};
use reth_rpc_eth_types::error::api::{FromEvmHalt, FromRevert};
use revm::context::{Block, BlockEnv, TxEnv, result::ExecutionResult};

/// Estimates gas for an EIP-8130 `eth_estimateGas` request by running a single
/// read-only [`base_common_evm::Eip8130Executor::simulate`] at the block state.
///
/// The EIP-8130 pipeline prices a deterministic, signature-independent amount
/// (intrinsic + phased-call gas + payer authentication), so a single
/// [`base_common_evm::Eip8130Executor::simulate`] resolves the sender and prices
/// intrinsic/auth gas once. To return a gas *limit* that is guaranteed to
/// succeed — covering both the unrefunded gross call spend and EIP-150's 63/64
/// retention across nested calls — `simulate` internally binary-searches the
/// minimum call pool at which the phased calls still succeed, re-dispatching them
/// at candidate pools over reverted journal checkpoints. The determinism keeps
/// that search to a handful of iterations (vs. the standard estimator searching
/// the whole gas limit from scratch). The simulation is built from an unsigned
/// request with a stub authentication blob and never commits state.
///
/// **Fork-agnostic on purpose.** This does not check Cobalt activation; callers
/// must gate via [`crate::Eip8130CobaltGate`] before invoking it.
///
/// **Revert semantics match standard `eth_estimateGas`.** If a phased call
/// reverts (or the simulation halts), this returns an execution error carrying
/// the revert data, exactly like the standard estimator. An EIP-8130
/// transaction whose phases revert is still *included* on-chain (nonce consumed,
/// fee paid), but surfacing the failure — rather than a gas number for a call
/// that would not succeed — keeps estimation consistent with what callers and
/// tooling expect from `eth_estimateGas`/`eth_call`.
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
    /// - An execution error (carrying revert data) if a phased call reverts or
    ///   the simulation halts, matching standard `eth_estimateGas`.
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
        // Surface phase reverts/halts as execution errors, like the standard
        // estimator (`FullEthApi` already guarantees these on `Eth::Error`).
        Eth::Error: FromRevert + FromEvmHalt<HaltReasonFor<Eth::Evm>>,
        ErrorObjectOwned: From<Eth::Error>,
    {
        let (evm_env, at) = eth_api.evm_env_at(block_id).await?;
        let chain_id = evm_env.cfg_env.chain_id;
        // Bound execution by the block gas limit when the request omits `gas`.
        let gas_cap = Block::gas_limit(&evm_env.block_env);

        let sim_tx = request.to_eip8130_simulation_tx(chain_id, gas_cap).ok_or_else(|| {
            ErrorObjectOwned::owned(
                INVALID_PARAMS_CODE,
                "invalid EIP-8130 estimate request: missing EIP-8130 fields, missing the required \
                 `from` sender, or a declared authentication size exceeds the maximum",
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

        // Mirror `eth_estimateGas`: a phase revert (or halt) is a failure. The
        // EIP-8130 transaction would still be included on-chain, but reporting
        // the failure with its revert data — rather than a gas number for a call
        // that would not succeed — keeps estimation consistent with the standard
        // estimator and surfaces the reason to callers.
        let gas_used = result.result.tx_gas_used();
        match result.result {
            ExecutionResult::Success { .. } => Ok(U256::from(gas_used)),
            ExecutionResult::Revert { output, .. } => {
                Err(<Eth::Error as FromRevert>::from_revert(output).into())
            }
            ExecutionResult::Halt { reason, gas, .. } => {
                Err(<Eth::Error as FromEvmHalt<_>>::from_evm_halt(reason, gas.tx_gas_used()).into())
            }
        }
    }
}
