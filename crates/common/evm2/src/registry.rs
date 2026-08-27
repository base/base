//! Base transaction registry.

use alloy_primitives::TxKind;
use evm2::{
    TxResult,
    env::TxEnvExt,
    interpreter::{Host, MessageExt},
    registry::{HandlerError, HandlerResult, TxRegistry, TxRequest},
};

use crate::{
    BaseEvmTypes,
    transaction::{BaseTransaction, DEPOSIT_TX_TYPE, DepositTransaction},
};

/// Error returned when a create-kind deposit transaction is encountered.
///
/// Create-kind deposits require contract-address derivation and a create message
/// frame, which are follow-up work; until then they are rejected explicitly.
#[derive(Debug)]
pub struct CreateDepositUnsupported;

impl core::fmt::Display for CreateDepositUnsupported {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("create-kind deposit transactions are not yet supported")
    }
}

impl core::error::Error for CreateDepositUnsupported {}

impl BaseEvmTypes {
    /// Builds the Base transaction registry.
    ///
    /// Registers the OP-stack deposit handler (type `0x7e`). Standard Ethereum
    /// transaction handlers — wired with
    /// [`BaseTxHandlerHooks`](crate::BaseTxHandlerHooks) for L1 fee settlement —
    /// are registered here in follow-up work.
    pub fn tx_registry() -> TxRegistry<Self, TxResult<Self>> {
        TxRegistry::new().with_handler(
            DEPOSIT_TX_TYPE,
            BaseTransaction::as_deposit,
            Self::handle_deposit,
        )
    }

    /// Executes an OP-stack deposit transaction.
    ///
    /// Deposits mint value on L2 and execute exempt from the L1 data fee and
    /// standard gas payment. This currently runs the deposit as a plain call and
    /// is the anchor for the remaining deposit semantics: crediting `mint`,
    /// honoring `is_system_transaction`, resolving the destination's (possibly
    /// delegated) code, and handling create-kind deposits.
    pub fn handle_deposit(
        req: TxRequest<'_, '_, Self, DepositTransaction>,
    ) -> HandlerResult<TxResult<Self>> {
        let destination = match req.tx.to {
            TxKind::Call(address) => address,
            // Create-kind deposits require contract-address derivation and a create
            // message frame (follow-up work). Reject explicitly rather than silently
            // misrouting the call to the zero address.
            TxKind::Create => return Err(HandlerError::external(CreateDepositUnsupported)),
        };
        let mut message = MessageExt {
            gas_limit: req.tx.gas_limit,
            destination,
            caller: req.tx.from,
            input: req.tx.input.clone(),
            value: req.tx.value,
            ..MessageExt::default()
        };
        let tx_env = TxEnvExt::default();
        let result = req.host.execute_message(&tx_env, &mut message);
        Ok(TxResult::<Self> {
            status: result.stop.is_success(),
            total_gas_spent: req.tx.gas_limit.saturating_sub(result.gas.remaining()),
            stop: result.stop,
            output: result.output,
            ..Default::default()
        })
    }
}
