use std::future::Future;

use alloy_evm::overrides::{apply_block_overrides, apply_state_overrides};
use alloy_primitives::Bytes;
use alloy_rpc_types_eth::{BlockId, state::EvmOverrides};
use reth_evm::{TransactionEnvMut, env::BlockEnvironment};
use reth_provider::ProviderError;
use reth_revm::{cancelled::CancelOnDrop, database::StateProviderDatabase, db::State};
use reth_rpc_eth_api::{
    FromEvmError, RpcConvert, RpcTxReq,
    helpers::{Call, EthCall, LoadPendingBlock, LoadState, SpawnBlocking, estimate::EstimateCall},
};
use reth_rpc_eth_types::{EthApiError, cache::db::StateProviderTraitObjWrapper};
use revm::{Database, context::Block, context_interface::Transaction};
use tracing::{Instrument, Span, info_span, trace, warn};

use crate::{BaseEthApi, BaseEthApiError, eth::RpcNodeCore};

type BaseEthApiEvm<N, Rpc> = <BaseEthApi<N, Rpc> as RpcNodeCore>::Evm;

impl<N, Rpc> EthCall for BaseEthApi<N, Rpc>
where
    N: RpcNodeCore,
    BaseEthApiError: FromEvmError<BaseEthApiEvm<N, Rpc>> + From<ProviderError>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = BaseEthApiError, Evm = BaseEthApiEvm<N, Rpc>>,
{
    fn call(
        &self,
        request: RpcTxReq<<Self::RpcConvert as RpcConvert>::Network>,
        block_number: Option<BlockId>,
        overrides: EvmOverrides,
    ) -> impl Future<Output = Result<Bytes, Self::Error>> + Send {
        self.traced_eth_call(request, block_number.unwrap_or_default(), overrides)
    }
}

impl<N, Rpc> EstimateCall for BaseEthApi<N, Rpc>
where
    N: RpcNodeCore,
    BaseEthApiError: FromEvmError<BaseEthApiEvm<N, Rpc>> + From<ProviderError>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = BaseEthApiError, Evm = BaseEthApiEvm<N, Rpc>>,
{
}

impl<N, Rpc> Call for BaseEthApi<N, Rpc>
where
    N: RpcNodeCore,
    BaseEthApiError: FromEvmError<BaseEthApiEvm<N, Rpc>> + From<ProviderError>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = BaseEthApiError, Evm = BaseEthApiEvm<N, Rpc>>,
{
    #[inline]
    fn call_gas_limit(&self) -> u64 {
        self.inner.eth_api.gas_cap()
    }

    #[inline]
    fn max_simulate_blocks(&self) -> u64 {
        self.inner.eth_api.max_simulate_blocks()
    }

    #[inline]
    fn evm_memory_limit(&self) -> u64 {
        self.inner.eth_api.evm_memory_limit()
    }

    #[inline]
    fn compute_state_root_for_eth_simulate(&self) -> bool {
        self.inner.eth_api.compute_state_root_for_eth_simulate()
    }
}

impl<N, Rpc> BaseEthApi<N, Rpc>
where
    N: RpcNodeCore,
    BaseEthApiError: FromEvmError<BaseEthApiEvm<N, Rpc>> + From<ProviderError>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = BaseEthApiError, Evm = BaseEthApiEvm<N, Rpc>>,
    Self: reth_rpc_eth_api::EthApiTypes<Error = BaseEthApiError, RpcConvert = Rpc>,
    Self: LoadPendingBlock,
{
    /// Executes `eth_call` with Base-owned tracing around the major orchestration steps.
    #[doc(hidden)]
    pub fn traced_eth_call(
        &self,
        request: RpcTxReq<Rpc::Network>,
        block_number: BlockId,
        overrides: EvmOverrides,
    ) -> impl Future<Output = Result<Bytes, BaseEthApiError>> + Send {
        let has_state_overrides = overrides.state.is_some();
        let has_block_overrides = overrides.block.is_some();
        let span = info_span!(
            "eth_call",
            block = ?block_number,
            has_state_overrides,
            has_block_overrides,
        );

        async move {
            let _permit = self.acquire_owned_blocking_io().await;
            let guard = CancelOnDrop::default();
            let cancel = guard.clone();
            let call_span = Span::current();

            let (mut evm_env, at) = async { self.evm_env_at(block_number).await }
                .instrument(info_span!(parent: &call_span, "resolve_call_evm_env", block = ?block_number))
                .await?;

            let res = self
                .spawn_blocking_io_fut(async move |this| {
                    let state = async { this.state_at_block_id(at).await }
                        .instrument(info_span!(parent: &call_span, "load_call_state", block = ?at))
                        .await?;

                    let mut db = info_span!(parent: &call_span, "build_call_state_db", block = ?at)
                        .in_scope(|| {
                            State::builder()
                                .with_database(StateProviderDatabase::new(
                                    StateProviderTraitObjWrapper(state),
                                ))
                                .build()
                        });

                    let request_has_gas_limit = request.as_ref().gas.is_some();
                    let requested_gas = request.as_ref().gas;
                    let mut request = request;

                    let tx_env = info_span!(
                        parent: &call_span,
                        "prepare_call_env",
                        from = ?request.as_ref().from,
                        to = ?request.as_ref().to,
                        requested_gas = ?requested_gas,
                        has_state_overrides = overrides.state.is_some(),
                        has_block_overrides = overrides.block.is_some(),
                    )
                    .in_scope(|| -> Result<_, BaseEthApiError> {
                        if let Some(requested_gas) = request.as_ref().gas {
                            let global_gas_cap = this.inner.eth_api.gas_cap();
                            if global_gas_cap != 0 && global_gas_cap < requested_gas {
                                warn!(
                                    target: "rpc::eth::call",
                                    request = ?request,
                                    global_gas_cap,
                                    "capping gas limit to global gas cap",
                                );
                                request.as_mut().gas = Some(global_gas_cap);
                            }
                        } else {
                            request.as_mut().gas = Some(this.inner.eth_api.gas_cap());
                        }

                        evm_env.cfg_env.disable_block_gas_limit = true;
                        evm_env.cfg_env.disable_eip3607 = true;
                        evm_env.cfg_env.disable_base_fee = true;
                        evm_env.cfg_env.tx_gas_limit_cap = Some(u64::MAX);
                        evm_env.cfg_env.disable_fee_charge = true;
                        evm_env.cfg_env.memory_limit = this.inner.eth_api.evm_memory_limit();
                        request.as_mut().nonce = None;

                        if let Some(block_overrides) = overrides.block {
                            info_span!(parent: &call_span, "apply_call_block_overrides")
                                .in_scope(|| {
                                    apply_block_overrides(
                                        *block_overrides,
                                        &mut db,
                                        evm_env.block_env.inner_mut(),
                                    );
                                });
                        }
                        if let Some(state_overrides) = overrides.state {
                            info_span!(parent: &call_span, "apply_call_state_overrides")
                                .in_scope(|| apply_state_overrides(state_overrides, &mut db))
                                .map_err(EthApiError::from_state_overrides_err)?;
                        }

                        let mut tx_env =
                            info_span!(parent: &call_span, "build_call_tx_env").in_scope(
                                || -> Result<_, BaseEthApiError> {
                                    if request.as_ref().nonce.is_none() {
                                        let caller = request.as_ref().from.unwrap_or_default();
                                        let nonce = info_span!(parent: &call_span, "load_call_nonce", caller = %caller)
                                            .in_scope(|| {
                                                db.basic(caller)
                                                    .map_err(EthApiError::from)
                                                    .map(|account| {
                                                        account
                                                            .map(|account| account.nonce)
                                                            .unwrap_or_default()
                                                    })
                                            })?;
                                        request.as_mut().nonce = Some(nonce);
                                    }

                                    info_span!(parent: &call_span, "convert_call_tx_env")
                                        .in_scope(|| Call::create_txn_env(&this, &evm_env, request, &mut db))
                                },
                            )?;

                        if tx_env.gas_price() == 0 {
                            evm_env.block_env.inner_mut().basefee = 0;
                        }

                        if !request_has_gas_limit && tx_env.gas_price() > 0 {
                            trace!(
                                target: "rpc::eth::call",
                                tx_env = ?tx_env,
                                "applying gas limit cap with caller allowance",
                            );
                            let cap = info_span!(
                                parent: &call_span,
                                "cap_call_gas_limit_with_allowance",
                            )
                            .in_scope(|| Call::caller_gas_allowance(&this, &mut db, &evm_env, &tx_env))?;
                            tx_env.set_gas_limit(cap.min(evm_env.block_env.gas_limit()));
                        }

                        Ok(tx_env)
                    })?;

                    if cancel.is_cancelled() {
                        return Err(EthApiError::InternalEthError.into())
                    }

                    let res = info_span!(parent: &call_span, "execute_eth_call", block = ?at)
                        .in_scope(|| Call::transact(&this, &mut db, evm_env, tx_env))?;

                    <BaseEthApiError as FromEvmError<BaseEthApiEvm<N, Rpc>>>::ensure_success(
                        res.result,
                    )
                })
                .await;

            drop(guard);
            res
        }
        .instrument(span)
    }
}
