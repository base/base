//! Base block executor for EVM2.

use alloy_consensus::{Eip658Value, Receipt, transaction::Recovered};
use alloy_eips::eip2718::Typed2718;
use base_common_consensus::{BaseReceiptEnvelope, DepositReceipt};
use base_common_genesis::BaseUpgrade;
use evm2::{
    BlockStateAccumulator, Evm,
    registry::{HandlerError, HandlerResult},
};

use crate::{BaseEvmTypes, transaction::BaseTxEnvelope};

/// Error returned when a block's cumulative gas used would overflow `u64`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CumulativeGasOverflow;

impl core::fmt::Display for CumulativeGasOverflow {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("block cumulative gas used overflowed u64")
    }
}

impl core::error::Error for CumulativeGasOverflow {}

/// The outcome of executing a block's transactions: one receipt per transaction (in order) and
/// the total gas used.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlockExecutionResult {
    /// The transaction receipts, in execution order.
    pub receipts: Vec<BaseReceiptEnvelope>,
    /// The block's cumulative gas used (the last receipt's cumulative gas, or zero).
    pub gas_used: u64,
}

/// Executes a Base block's transactions on an EVM2 [`Evm`], building receipts and accumulating
/// the block's state delta.
///
/// This is the transaction-execution core: it runs each transaction through the registry
/// (deposit or standard handler), builds its receipt with running cumulative gas, and commits
/// its state into a [`BlockStateAccumulator`]. Pre-execution block-boundary system calls
/// (EIP-4788/EIP-2935, Canyon create2-deployer, Cobalt system accounts), the EIP-8130 path, and
/// the Jovian DA-footprint checks are layered on in follow-up work.
#[derive(Debug)]
pub struct BaseBlockExecutor<'a> {
    evm: Evm<'a, BaseEvmTypes>,
    block_state: BlockStateAccumulator,
    receipts: Vec<BaseReceiptEnvelope>,
    gas_used: u64,
}

impl<'a> BaseBlockExecutor<'a> {
    /// Creates a block executor over `evm`.
    pub fn new(evm: Evm<'a, BaseEvmTypes>) -> Self {
        Self { evm, block_state: BlockStateAccumulator::new(), receipts: Vec::new(), gas_used: 0 }
    }

    /// Returns the EVM.
    pub const fn evm(&self) -> &Evm<'a, BaseEvmTypes> {
        &self.evm
    }

    /// Returns a mutable reference to the EVM, e.g. to seed accounts before execution.
    pub const fn evm_mut(&mut self) -> &mut Evm<'a, BaseEvmTypes> {
        &mut self.evm
    }

    /// Executes `tx`, appending its receipt (with running cumulative gas) and committing its
    /// state changes into the block-state accumulator.
    pub fn execute_transaction(&mut self, tx: &Recovered<BaseTxEnvelope>) -> HandlerResult<()> {
        let ty = tx.ty();
        let is_deposit = tx.is_deposit();
        let signer = tx.signer();

        // The deposit receipt records the depositor's nonce *before* the deposit executes (and
        // bumps it), read untracked so it does not perturb execution. Matches the reference.
        let deposit_nonce = if is_deposit {
            let nonce = self
                .evm
                .state_mut()
                .account_info_untracked(&signer)
                .map_err(HandlerError::Fatal)?
                .map(|info| info.nonce)
                .unwrap_or_default();
            Some(nonce)
        } else {
            None
        };

        let executed = self.evm.transact(tx)?;
        let result = executed.result();
        let success = result.status;
        let logs = result.logs.clone();
        // Fail on overflow rather than silently saturating, which would corrupt this and every
        // later receipt's cumulative gas. Practically unreachable under block gas limits, but
        // this executor has no pre-execution block-gas-limit guard yet.
        self.gas_used = self
            .gas_used
            .checked_add(result.tx_gas_used())
            .ok_or_else(|| HandlerError::external(CumulativeGasOverflow))?;
        let cumulative_gas_used = self.gas_used;
        let _ = executed.commit_to(&mut self.block_state);

        let receipt = Receipt { status: Eip658Value::Eip658(success), cumulative_gas_used, logs };
        let envelope = if is_deposit {
            // The deposit receipt version is set once Canyon is active.
            let canyon = (self.evm.config_spec_id().upgrade() as u8) >= (BaseUpgrade::Canyon as u8);
            let deposit_receipt_version = canyon.then_some(1);
            BaseReceiptEnvelope::Deposit(
                DepositReceipt { inner: receipt, deposit_nonce, deposit_receipt_version }
                    .with_bloom(),
            )
        } else {
            let receipt = receipt.with_bloom();
            match ty {
                1 => BaseReceiptEnvelope::Eip2930(receipt),
                2 => BaseReceiptEnvelope::Eip1559(receipt),
                4 => BaseReceiptEnvelope::Eip7702(receipt),
                // 0x79 is the enshrined EIP-8130 account-abstraction transaction.
                0x79 => BaseReceiptEnvelope::Eip8130(receipt),
                // Legacy (type 0) uses the legacy receipt shape. These type bytes mirror the
                // handlers registered in tx_registry(); the assert flags any registered type that
                // gains a handler but not a receipt arm here (which would mis-type its receipt).
                other => {
                    debug_assert_eq!(other, 0, "unmapped standard tx type {other:#x} in receipts");
                    BaseReceiptEnvelope::Legacy(receipt)
                }
            }
        };
        self.receipts.push(envelope);
        Ok(())
    }

    /// Finalizes the block, returning the EVM, the execution result (receipts + gas used), and
    /// the accumulated block-state delta.
    pub fn finish(self) -> (Evm<'a, BaseEvmTypes>, BlockExecutionResult, BlockStateAccumulator) {
        let result = BlockExecutionResult { receipts: self.receipts, gas_used: self.gas_used };
        (self.evm, result, self.block_state)
    }
}
