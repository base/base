use alloy_consensus::TxEip2930;
use alloy_primitives::U256;

use super::{
    access_list_counts, execute_initial_frame, floor_gas, initial_gas_and_reservoir, intrinsic_gas,
    prepare_initial_frame, validate_block_gas_limit, validate_chain_id, validate_create_initcode,
    validate_execution_gas_limit_cap, validate_floor_gas, validate_gas_price,
    validate_intrinsic_gas, validate_nonce_not_overflow, validate_sender,
    validate_tx_gas_limit_cap, warm_access_list, warm_base_accounts,
};
use crate::{
    EvmTypes, TxResult,
    env::TxEnvExt,
    evm::{
        error_handler,
        handler::{DefaultTxHandlerHooks, GasSettlement, TxHandlerHooks},
    },
    interpreter::GasTracker,
    registry::{HandlerResult, TxRequest},
};

/// Executes an EIP-2930 transaction using Ethereum rules.
pub fn handle<T: EvmTypes>(req: TxRequest<'_, '_, T, TxEip2930>) -> HandlerResult<TxResult<T>> {
    handle_with_hooks::<T, DefaultTxHandlerHooks>(req)
}

/// Executes an EIP-2930 transaction using Ethereum rules and custom handler hooks.
pub fn handle_with_hooks<T: EvmTypes, H: TxHandlerHooks<T>>(
    req: TxRequest<'_, '_, T, TxEip2930>,
) -> HandlerResult<TxResult<T>> {
    let caller = req.tx.signer();
    let tx = req.tx.inner();
    let gas_price = U256::from(tx.gas_price);

    validate_gas_price(req.host.version(), gas_price, req.host.block.basefee)?;
    validate_chain_id(req.host.version(), Some(tx.chain_id), false)?;
    validate_tx_gas_limit_cap(req.host.version(), tx.gas_limit)?;
    validate_block_gas_limit(req.host.version(), tx.gas_limit, req.host.block.gas_limit)?;
    validate_create_initcode(req.host.version(), tx.to, &tx.input)?;
    validate_nonce_not_overflow(tx.nonce)?;
    let (access_list_accounts, access_list_storage_keys) = access_list_counts(&tx.access_list);
    let mut intrinsic = intrinsic_gas(
        req.host.version(),
        caller,
        tx.to,
        &tx.input,
        access_list_accounts,
        access_list_storage_keys,
        tx.value,
    );
    // EIP-2780: state-dependent gas is charged at the runtime gas phase, not the intrinsic phase.
    let mut initial_state_gas = 0;
    H::adjust_intrinsic_gas(req.host, req.envelope, &mut intrinsic, &mut initial_state_gas)?;
    validate_intrinsic_gas(tx.gas_limit, intrinsic, initial_state_gas)?;
    let floor_gas = floor_gas(
        req.host.version(),
        caller,
        tx.to,
        &tx.input,
        access_list_accounts,
        access_list_storage_keys,
        tx.value,
    );
    validate_floor_gas(tx.gas_limit, floor_gas)?;
    validate_execution_gas_limit_cap(req.host.version(), tx.gas_limit, intrinsic, floor_gas)?;

    let max_gas_cost = U256::from(tx.gas_limit) * gas_price;
    validate_sender(req.host, caller, tx.nonce, max_gas_cost.saturating_add(tx.value))?;

    warm_base_accounts(req.host, caller, tx.to);
    warm_access_list(req.host, &tx.access_list);

    req.host.state.account(&caller, false).map_err(error_handler!(req.host))?.bump_nonce();
    H::before_execution(req.host, req.envelope, caller, max_gas_cost)?;

    let (execution_gas_limit, reservoir) =
        initial_gas_and_reservoir(req.host.version(), tx.gas_limit, intrinsic, initial_state_gas);
    let tx_env = TxEnvExt {
        origin: caller,
        gas_price,
        chain_id: U256::from(req.host.version().chain_id),
        ..TxEnvExt::default()
    };
    let mut tx_gas =
        GasTracker::new_with_execution_gas_and_reservoir(execution_gas_limit, reservoir);
    let frame =
        prepare_initial_frame(req.host, caller, tx.nonce, tx.to, &tx.input, tx.value, &mut tx_gas)?;
    let result = execute_initial_frame(
        req.host,
        &tx_env,
        frame,
        &mut tx_gas,
        execution_gas_limit,
        reservoir,
    );
    H::settle_transaction(
        req.host,
        req.envelope,
        GasSettlement {
            caller,
            gas_price,
            gas_limit: tx.gas_limit,
            floor_gas,
            initial_state_gas,
            state_refund: 0,
            result,
        },
    )
}
