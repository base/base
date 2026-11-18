//! `eth_estimateGas` gas estimation for Base transaction simulation requests.

use std::fmt::Display;

use alloy_eips::BlockId;
use alloy_evm::{
    EvmFactory,
    overrides::{apply_block_overrides, apply_state_overrides},
};
use alloy_primitives::{Bytes, U256};
use alloy_rpc_types::state::EvmOverrides;
use base_common_evm::{BaseTransaction as BaseRevm, TxBaseExecutionMode};
use base_common_rpc_types::{BaseRpcTypes, BaseTransactionRequest};
use base_execution_base_tx::NonceValidator;
use base_execution_chainspec::BaseChainSpec;
use base_execution_evm::{BaseNextBlockEnvAttributes, BasePendingForecast};
use jsonrpsee_types::{ErrorObjectOwned, error::INVALID_PARAMS_CODE};
use reth_chainspec::ChainSpecProvider;
use reth_errors::RethError;
use reth_evm::{ConfigureEvm, EvmFactoryFor, HaltReasonFor, TxEnvFor};
use reth_rpc_eth_api::{FromEthApiError, helpers::FullEthApi};
use reth_rpc_eth_types::{
    EthApiError,
    error::api::{FromEvmHalt, FromRevert},
};
use revm::context::{
    Block, BlockEnv, Cfg, TxEnv,
    result::{ExecutionResult, Output},
};

/// Estimates gas for a Base transaction `eth_estimateGas` request by running a single
/// read-only [`base_common_evm::TxBaseExecutor::simulate`] at the block state.
///
/// The Base transaction pipeline prices a deterministic, signature-independent amount
/// (intrinsic + phased-call gas + payer authentication), so a single
/// [`base_common_evm::TxBaseExecutor::simulate`] resolves the sender and prices
/// intrinsic/auth gas once. To return a gas *limit* that is guaranteed to
/// succeed — covering both the unrefunded gross call spend and EIP-150's 63/64
/// retention across nested calls — `simulate` internally binary-searches the
/// minimum call pool at which the phased calls still succeed, re-dispatching them
/// at candidate pools over reverted journal checkpoints. The determinism keeps
/// that search to a handful of iterations (vs. the standard estimator searching
/// the whole gas limit from scratch). The simulation is built from an unsigned
/// request with a stub authentication blob and never commits state.
///
/// **Fork-agnostic on purpose.** This does not check Everest activation; callers
/// must gate via [`crate::TxBaseEverestGate`] before invoking it.
///
/// **Revert semantics match standard `eth_estimateGas`.** If a phased call
/// reverts (or the simulation halts), this returns an execution error carrying
/// the revert data, exactly like the standard estimator. A Base
/// transaction whose phases revert is still *included* on-chain (nonce consumed,
/// fee paid), but surfacing the failure — rather than a gas number for a call
/// that would not succeed — keeps estimation consistent with what callers and
/// tooling expect from `eth_estimateGas`/`eth_call`.
#[derive(Debug)]
pub struct TxBaseGasEstimator;

impl TxBaseGasEstimator {
    /// Resolves the EVM environment at `block_id`, builds the unsigned
    /// simulation transaction, applies any `overrides` (block then state, to
    /// match the standard call path), and runs the Base transaction simulation,
    /// returning the gas it would charge.
    ///
    /// Like the standard estimator, `pending` without an executed pending block
    /// simulates the scheduled Denim successor, including its temporary
    /// `BaseTime` state beneath user state overrides.
    ///
    /// Block overrides are threaded through (not just state overrides) so the
    /// simulation runs against the same block env — basefee, timestamp, etc. —
    /// as the standard `eth_estimateGas` path.
    ///
    /// # Errors
    /// - `INVALID_PARAMS` if `request` carries no Base transaction fields (callers
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
        Eth: FullEthApi<NetworkTypes = BaseRpcTypes>,
        Eth::Evm: ConfigureEvm<NextBlockEnvCtx = BaseNextBlockEnvAttributes>,
        Eth::Provider: ChainSpecProvider<ChainSpec = BaseChainSpec>,
        TxEnvFor<Eth::Evm>: From<BaseRevm<TxEnv>>,
        // Pin the block env to revm's concrete type so block overrides can be
        // applied directly (Base's `EvmFactory::BlockEnv` is `revm::BlockEnv`).
        EvmFactoryFor<Eth::Evm>: EvmFactory<BlockEnv = BlockEnv>,
        ErrorObjectOwned: From<Eth::Error>,
    {
        let result =
            Self::simulate(eth_api, request, block_id, overrides, TxBaseExecutionMode::Simulate)
                .await?;
        // Mirror `eth_estimateGas`: a phase revert (or halt) is a failure. The
        // Base transaction would still be included on-chain, but reporting
        // the failure with its revert data — rather than a gas number for a call
        // that would not succeed — keeps estimation consistent with the standard
        // estimator and surfaces the reason to callers.
        let gas_used = result.tx_gas_used();
        Self::success::<Eth>(result).map(|_| U256::from(gas_used))
    }

    /// Runs a Base transaction `eth_call`: the same read-only simulation as
    /// [`Self::estimate`], except the calls execute once at the request's gas
    /// (capped as for [`Self::estimate`]) with no gas-limit search, returning the
    /// output of the final call. A phase revert or halt is an execution error
    /// carrying the revert data, as for a standard `eth_call`.
    ///
    /// # Errors
    /// The same as [`Self::estimate`].
    pub async fn call<Eth>(
        eth_api: &Eth,
        request: BaseTransactionRequest,
        block_id: BlockId,
        overrides: EvmOverrides,
    ) -> Result<Bytes, ErrorObjectOwned>
    where
        Eth: FullEthApi<NetworkTypes = BaseRpcTypes>,
        Eth::Evm: ConfigureEvm<NextBlockEnvCtx = BaseNextBlockEnvAttributes>,
        Eth::Provider: ChainSpecProvider<ChainSpec = BaseChainSpec>,
        TxEnvFor<Eth::Evm>: From<BaseRevm<TxEnv>>,
        // Pin the block env to revm's concrete type so block overrides can be
        // applied directly (Base's `EvmFactory::BlockEnv` is `revm::BlockEnv`).
        EvmFactoryFor<Eth::Evm>: EvmFactory<BlockEnv = BlockEnv>,
        ErrorObjectOwned: From<Eth::Error>,
    {
        let result =
            Self::simulate(eth_api, request, block_id, overrides, TxBaseExecutionMode::Call)
                .await?;
        Self::success::<Eth>(result).map(Output::into_data)
    }

    /// The success output of a simulation, or the standard revert or halt error.
    fn success<Eth>(
        result: ExecutionResult<HaltReasonFor<Eth::Evm>>,
    ) -> Result<Output, ErrorObjectOwned>
    where
        Eth: FullEthApi,
        ErrorObjectOwned: From<Eth::Error>,
    {
        match result {
            ExecutionResult::Success { output, .. } => Ok(output),
            ExecutionResult::Revert { output, .. } => {
                Err(<Eth::Error as FromRevert>::from_revert(output).into())
            }
            ExecutionResult::Halt { reason, gas, .. } => {
                Err(<Eth::Error as FromEvmHalt<_>>::from_evm_halt(reason, gas.tx_gas_used()).into())
            }
        }
    }

    /// Builds the simulation transaction for `request`, checks it against the
    /// rules pool admission applies, and runs it in `mode` at `block_id` with
    /// `overrides` on the blocking pool.
    ///
    /// Holds a blocking-IO permit until the blocking simulation finishes, as the
    /// standard `eth_call` does, so concurrent Base transaction simulations are bounded
    /// by the same limit as every other EVM-executing request, even after the
    /// client disconnects.
    async fn simulate<Eth>(
        eth_api: &Eth,
        request: BaseTransactionRequest,
        block_id: BlockId,
        mut overrides: EvmOverrides,
        mode: TxBaseExecutionMode,
    ) -> Result<ExecutionResult<HaltReasonFor<Eth::Evm>>, ErrorObjectOwned>
    where
        Eth: FullEthApi<NetworkTypes = BaseRpcTypes>,
        Eth::Evm: ConfigureEvm<NextBlockEnvCtx = BaseNextBlockEnvAttributes>,
        Eth::Provider: ChainSpecProvider<ChainSpec = BaseChainSpec>,
        TxEnvFor<Eth::Evm>: From<BaseRevm<TxEnv>>,
        // Pin the block env to revm's concrete type so block overrides can be
        // applied directly (Base's `EvmFactory::BlockEnv` is `revm::BlockEnv`).
        EvmFactoryFor<Eth::Evm>: EvmFactory<BlockEnv = BlockEnv>,
        ErrorObjectOwned: From<Eth::Error>,
    {
        let permit = eth_api
            .acquire_owned_blocking_io()
            .await
            .map_err(|_| Eth::Error::from_eth_err(EthApiError::InternalEthError))?;
        let (evm_env, at, forecast) = BasePendingForecast::evm_env_at(eth_api, block_id).await?;
        let chain_id = evm_env.cfg_env.chain_id;
        // Bound execution like standard `eth_estimateGas`: the request's `gas`,
        // or the default when omitted, never exceeds the block gas limit, the
        // EIP-7825 per-transaction cap, or the operator's `--rpc.gascap` (zero
        // means uncapped). An unbounded `gas` would let one request run its
        // calls (twice, if they fail) at any gas the caller picks.
        let mut gas_cap =
            Block::gas_limit(&evm_env.block_env).min(evm_env.cfg_env.tx_gas_limit_cap());
        let rpc_gas_cap = eth_api.call_gas_limit();
        if rpc_gas_cap != 0 {
            gas_cap = gas_cap.min(rpc_gas_cap);
        }

        let mut sim_tx =
            request.to_base_tx_simulation_tx(chain_id, gas_cap).map_err(Self::invalid_request)?;
        // The validity window and nonce-free rules, as pool admission applies
        // them at the simulated block's timestamp, and the rule that a
        // delegation uses the protocol nonce. The simulation does not validate
        // the nonce, so it would otherwise accept a delegation that pool
        // admission and inclusion always reject. A `time` block override is
        // only applied to the env inside the state closure below, so it must be
        // honored here explicitly.
        let timestamp = overrides
            .block
            .as_ref()
            .and_then(|block| block.time)
            .unwrap_or_else(|| evm_env.block_env.timestamp.saturating_to());
        let now_ms = timestamp.saturating_mul(1_000);
        if let Some(parts) = &sim_tx.base_tx {
            parts.signed.validate_timestamp(now_ms).map_err(Self::invalid_request)?;
            NonceValidator::validate_delegation(parts.signed.tx())
                .map_err(Self::invalid_request)?;
        }
        if let Some(parts) = sim_tx.base_tx.as_mut() {
            parts.mode = mode;
        }

        let result = eth_api
            .spawn_with_state_at_block(at, move |this, mut db| {
                let _permit = permit;
                let mut evm_env = evm_env;
                // Block overrides first (mutating the block env), then state, so
                // the simulation matches the standard call path's ordering.
                if let Some(block) = overrides.block.take() {
                    apply_block_overrides(*block, &mut db, &mut evm_env.block_env);
                }
                if let Some(forecast) = forecast {
                    overrides = forecast
                        .state_overrides(this.provider().chain_spec().as_ref(), &mut db, overrides)
                        .map_err(RethError::other)
                        .map_err(Eth::Error::from_eth_err)?;
                }
                if let Some(state) = overrides.state {
                    apply_state_overrides(state, &mut db).map_err(Eth::Error::from_eth_err)?;
                }
                this.transact(db, evm_env, sim_tx.into())
            })
            .await?;

        Ok(result.result)
    }

    /// The `INVALID_PARAMS` error for a request that cannot be simulated.
    fn invalid_request(error: impl Display) -> ErrorObjectOwned {
        ErrorObjectOwned::owned(
            INVALID_PARAMS_CODE,
            format!("invalid Base transaction estimate request: {error}"),
            None::<()>,
        )
    }
}
