//! `eth_estimateGas` gas estimation for EIP-8130 simulation requests.

use alloy_eips::BlockId;
use alloy_primitives::{Bytes, U256};
use alloy_rpc_types::state::EvmOverrides;
use base_common_evm2::{BaseEvmTypes, BaseTxEnvelope as BaseTxEnv, Eip8130SimulationMode};
use base_common_rpc_types::{BaseRpcTypes, BaseTransactionRequest};
use base_execution_chainspec::BaseChainSpec;
use base_execution_evm::{BaseNextBlockEnvAttributes, BasePendingForecast};
use evm2::TxResult;
use jsonrpsee_types::{ErrorObjectOwned, error::INVALID_PARAMS_CODE};
use reth_chainspec::ChainSpecProvider;
use reth_errors::RethError;
use reth_evm::{BlockExecutorFactory, ConfigureEvm, EvmEnv};
use reth_rpc_eth_api::{FromEthApiError, FromEvmError, helpers::FullEthApi};
use reth_rpc_eth_types::{
    EthApiError,
    cache::db::{apply_block_overrides, apply_state_overrides},
};

/// Estimates gas for an EIP-8130 `eth_estimateGas` request by running a single
/// read-only EIP-8130 simulation ([`base_common_evm2::Eip8130Simulation`]) at the block state.
///
/// The EIP-8130 pipeline prices a deterministic, signature-independent amount
/// (intrinsic + phased-call gas + payer authentication), so a single simulation
/// resolves the sender and prices intrinsic/auth gas once. To return a gas *limit* that is
/// guaranteed to succeed — covering both the unrefunded gross call spend and EIP-150's 63/64
/// retention across nested calls — the simulation internally binary-searches the
/// minimum call pool at which the phased calls still succeed, re-dispatching them
/// at candidate pools over reverted journal checkpoints. The determinism keeps
/// that search to a handful of iterations (vs. the standard estimator searching
/// the whole gas limit from scratch). The simulation is built from an unsigned
/// request with a stub authentication blob and never commits state.
///
/// **Fork-agnostic on purpose.** This does not check Everest activation; callers
/// must gate via [`crate::Eip8130EverestGate`] before invoking it.
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
    /// Like the standard estimator, `pending` without an executed pending block
    /// simulates the scheduled Denim successor, including its temporary
    /// `BaseTime` state beneath user state overrides.
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
        Eth: FullEthApi<NetworkTypes = BaseRpcTypes>,
        Eth::Evm: ConfigureEvm<
                NextBlockEnvCtx = BaseNextBlockEnvAttributes,
                BlockExecutorFactory: BlockExecutorFactory<EvmTypes = BaseEvmTypes>,
            >,
        Eth::Provider: ChainSpecProvider<ChainSpec = BaseChainSpec>,
        ErrorObjectOwned: From<Eth::Error>,
    {
        let result =
            Self::simulate(eth_api, request, block_id, overrides, Eip8130SimulationMode::Estimate)
                .await?;
        // Mirror `eth_estimateGas`: a phase revert (or halt) is a failure. The
        // EIP-8130 transaction would still be included on-chain, but reporting
        // the failure with its revert data — rather than a gas number for a call
        // that would not succeed — keeps estimation consistent with the standard
        // estimator and surfaces the reason to callers.
        let gas_used = result.tx_gas_used();
        Eth::Error::ensure_success::<BaseEvmTypes>(result)
            .map_err(Into::into)
            .map(|_| U256::from(gas_used))
    }

    /// Runs an EIP-8130 `eth_call`: the same read-only simulation as
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
        Eth::Evm: ConfigureEvm<
                NextBlockEnvCtx = BaseNextBlockEnvAttributes,
                BlockExecutorFactory: BlockExecutorFactory<EvmTypes = BaseEvmTypes>,
            >,
        Eth::Provider: ChainSpecProvider<ChainSpec = BaseChainSpec>,
        ErrorObjectOwned: From<Eth::Error>,
    {
        let result =
            Self::simulate(eth_api, request, block_id, overrides, Eip8130SimulationMode::Call)
                .await?;
        Eth::Error::ensure_success::<BaseEvmTypes>(result).map_err(Into::into)
    }

    /// Builds the simulation transaction for `request`, checks it against the
    /// rules pool admission applies, and runs it in `mode` at `block_id` with
    /// `overrides` on the blocking pool.
    ///
    /// Holds a blocking-IO permit until the blocking simulation finishes, as the
    /// standard `eth_call` does, so concurrent EIP-8130 simulations are bounded
    /// by the same limit as every other EVM-executing request, even after the
    /// client disconnects.
    async fn simulate<Eth>(
        eth_api: &Eth,
        request: BaseTransactionRequest,
        block_id: BlockId,
        mut overrides: EvmOverrides,
        mode: Eip8130SimulationMode,
    ) -> Result<TxResult<BaseEvmTypes>, ErrorObjectOwned>
    where
        Eth: FullEthApi<NetworkTypes = BaseRpcTypes>,
        Eth::Evm: ConfigureEvm<
                NextBlockEnvCtx = BaseNextBlockEnvAttributes,
                BlockExecutorFactory: BlockExecutorFactory<EvmTypes = BaseEvmTypes>,
            >,
        Eth::Provider: ChainSpecProvider<ChainSpec = BaseChainSpec>,
        ErrorObjectOwned: From<Eth::Error>,
    {
        let permit = eth_api
            .acquire_owned_blocking_io()
            .await
            .map_err(|_| Eth::Error::from_eth_err(EthApiError::InternalEthError))?;
        let (evm_env, at, forecast) = BasePendingForecast::evm_env_at(eth_api, block_id).await?;
        let chain_id = evm_env.chain_id();
        // Bound execution like standard `eth_estimateGas`: the request's `gas`,
        // or the default when omitted, never exceeds the block gas limit, the
        // EIP-7825 per-transaction cap, or the operator's `--rpc.gascap` (zero
        // means uncapped). An unbounded `gas` would let one request run its
        // calls (twice, if they fail) at any gas the caller picks.
        let mut gas_cap = evm_env
            .block_env()
            .gas_limit
            .saturating_to::<u64>()
            .min(evm_env.version().tx_gas_limit_cap);
        let rpc_gas_cap = eth_api.call_gas_limit();
        if rpc_gas_cap != 0 {
            gas_cap = gas_cap.min(rpc_gas_cap);
        }

        let sim_tx =
            request.to_eip8130_simulation_tx(chain_id, gas_cap, mode).map_err(|error| {
                ErrorObjectOwned::owned(
                    INVALID_PARAMS_CODE,
                    format!("invalid EIP-8130 estimate request: {error}"),
                    None::<()>,
                )
            })?;
        // The validity window and nonce-free rules, as pool admission applies
        // them at the simulated block's timestamp. A `time` block override is
        // only applied to the env inside the state closure below, so it must be
        // honored here explicitly.
        let timestamp = overrides
            .block
            .as_ref()
            .and_then(|block| block.time)
            .unwrap_or_else(|| evm_env.block_env().timestamp.saturating_to());
        let now_ms = timestamp.saturating_mul(1_000);
        if let BaseTxEnv::Eip8130(tx) = sim_tx.inner() {
            tx.signed.validate_timestamp(now_ms).map_err(|error| {
                ErrorObjectOwned::owned(
                    INVALID_PARAMS_CODE,
                    format!("invalid EIP-8130 estimate request: {error}"),
                    None::<()>,
                )
            })?;
        }

        let result = eth_api
            .spawn_with_state_at_block(at, move |this, mut db| {
                let _permit = permit;
                let mut evm_env = evm_env;
                // Block overrides first (mutating the block env), then state, so
                // the simulation matches the standard call path's ordering.
                if let Some(block) = overrides.block.take() {
                    apply_block_overrides(*block, &mut db, &mut evm_env);
                }
                if let Some(forecast) = forecast {
                    overrides = forecast
                        .state_overrides(this.provider().chain_spec().as_ref(), &mut db, overrides)
                        .map_err(RethError::other)
                        .map_err(Eth::Error::from_eth_err)?;
                }
                if let Some(state) = overrides.state {
                    apply_state_overrides(state, &mut db)
                        .map_err(EthApiError::from)
                        .map_err(Eth::Error::from_eth_err)?;
                }
                this.transact(db, evm_env, sim_tx)
            })
            .await?;

        Ok(result.result)
    }
}
