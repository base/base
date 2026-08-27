//! Base transaction handler hooks.

use alloy_consensus::Transaction;
use alloy_primitives::{Address, U256};
use evm2::{
    Evm, TxResult,
    ethereum::{charge_upfront, default_settle_gas},
    handler::{GasSettlement, TxHandlerHooks},
    interpreter::Host,
    registry::HandlerResult,
};

use crate::{BaseEvmTypes, transaction::BaseTransaction};

/// Base transaction handler hooks.
///
/// Charges the L1 data fee alongside the standard upfront gas cost for
/// non-deposit transactions; deposits are funded on L1 and exempt. All other
/// hooks keep the default Ethereum behavior.
#[derive(Clone, Copy, Debug, Default)]
pub struct BaseTxHandlerHooks;

impl BaseTxHandlerHooks {
    /// Returns the L1 data fee for `envelope` under the current block's
    /// L1 fee parameters. Deposits are funded on L1 and exempt.
    fn l1_fee(host: &mut Evm<'_, BaseEvmTypes>, envelope: &BaseTransaction) -> U256 {
        match envelope {
            // TODO: price over the full L1-posted (RLP-encoded) transaction, not
            // just the calldata, and switch to the Fjord FLZ estimate past that fork.
            BaseTransaction::Standard(tx) => host.block_env().ext.calculate_tx_l1_cost(tx.input()),
            // Deposits are funded on L1 and exempt from the L2 L1-data fee.
            BaseTransaction::Deposit(_) => U256::ZERO,
        }
    }
}

impl TxHandlerHooks<BaseEvmTypes> for BaseTxHandlerHooks {
    fn before_execution(
        host: &mut Evm<'_, BaseEvmTypes>,
        envelope: &BaseTransaction,
        caller: Address,
        upfront_fee: U256,
    ) -> HandlerResult<()> {
        // Compute the L1 fee once and stash it on the EVM extension so
        // `settle_transaction` records the exact value charged here, without a
        // second pass over the calldata or any risk of divergence if the block's
        // L1 fee parameters were mutated mid-transaction.
        let l1_fee = Self::l1_fee(host, envelope);
        host.ext_mut().l1_fee = l1_fee;
        charge_upfront(host, caller, upfront_fee.saturating_add(l1_fee))
    }

    fn settle_transaction(
        host: &mut Evm<'_, BaseEvmTypes>,
        _envelope: &BaseTransaction,
        gas: GasSettlement<BaseEvmTypes>,
    ) -> HandlerResult<TxResult<BaseEvmTypes>> {
        // Surface the L1 data fee charged in `before_execution` so downstream
        // receipt construction can report it. The default gas settlement is
        // otherwise unchanged.
        let l1_fee = host.ext().l1_fee;
        let mut result = default_settle_gas(host, gas)?;
        result.ext.l1_fee = l1_fee;
        Ok(result)
    }
}
