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
    transaction::{BaseTransaction, DEPOSIT_TX_TYPE, DepositTx},
};

/// Error returned when a create-kind deposit transaction is encountered.
///
/// Create-kind deposits require contract-address derivation and a create message
/// frame, which are follow-up work; until then they are rejected explicitly.
#[derive(Debug, thiserror::Error)]
#[error("create-kind deposit transactions are not yet supported")]
pub struct CreateDepositUnsupported;

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
        req: TxRequest<'_, '_, Self, DepositTx>,
    ) -> HandlerResult<TxResult<Self>> {
        let destination = match req.tx.to {
            TxKind::Call(address) => address,
            // Create-kind deposits require contract-address derivation and a create
            // message frame (follow-up work). Reject explicitly rather than silently
            // misrouting the call to the zero address.
            TxKind::Create => return Err(HandlerError::external(CreateDepositUnsupported)),
        };
        // `mint` is not yet credited to the sender and `is_system_transaction`
        // gas-metering exemption is not yet honored (both follow-up work, see the
        // doc comment above). Guard against silently mis-executing a deposit that
        // relies on either before support lands — these assertions catch unintended
        // use in tests without changing release behavior for the currently
        // exercised (mint-free, non-system) deposits.
        debug_assert_eq!(req.tx.mint, 0, "deposit mint is not yet credited before execution");
        debug_assert!(
            !req.tx.is_system_transaction,
            "system-transaction gas exemption is not yet honored"
        );
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
            // TODO: this gas accounting is incomplete per the OP-stack deposit
            // rules and will be finished alongside mint/system-transaction support:
            // system transactions (`is_system_transaction`) must report zero gas
            // spent, and failed (reverted) deposits are charged the full
            // `gas_limit` since there is no L2 fee payer to refund. Both directly
            // feed receipt `gasUsed` and the state root, so this must be exact
            // before the handler is wired into the node.
            total_gas_spent: req.tx.gas_limit.saturating_sub(result.gas.remaining()),
            stop: result.stop,
            output: result.output,
            ..Default::default()
        })
    }
}
