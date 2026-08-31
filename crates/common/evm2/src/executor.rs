//! Base block executor for EVM2.

use alloy_consensus::{Eip658Value, Receipt, transaction::Recovered};
use alloy_eips::eip2718::Typed2718;
use alloy_primitives::{B256, Bytes};
use base_common_consensus::{BaseReceiptEnvelope, DepositReceipt};
use base_common_genesis::BaseUpgrade;
use evm2::{
    BlockStateAccumulator, Evm, SpecId,
    evm::{BEACON_ROOTS_ADDRESS, HISTORY_STORAGE_ADDRESS, SystemTx},
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

/// Error returned when a transaction's reserved gas exceeds the block's remaining gas.
///
/// Mirrors the reference `BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas`: a
/// transaction whose gas limit is larger than the block's unused gas cannot be included. The
/// reserved gas is the transaction's gas limit (EIP-8130's additional payer-auth reservation does
/// not apply here, as that transaction type is not yet supported).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockGasLimitExceeded {
    /// The transaction's reserved gas (its gas limit).
    pub transaction_gas_limit: u64,
    /// The block's remaining available gas (block gas limit minus cumulative gas used).
    pub block_available_gas: u64,
}

impl core::fmt::Display for BlockGasLimitExceeded {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "transaction gas limit {} is more than the block's available gas {}",
            self.transaction_gas_limit, self.block_available_gas
        )
    }
}

impl core::error::Error for BlockGasLimitExceeded {}

/// Block-boundary context for pre-execution system calls.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BaseBlockExecutionCtx {
    /// The parent block hash, stored by the EIP-2935 block-hashes contract (Prague onwards).
    pub parent_hash: B256,
    /// The parent beacon block root, stored by the EIP-4788 beacon-roots contract (Cancun
    /// onwards). `None` when the block carries no beacon root.
    pub parent_beacon_block_root: Option<B256>,
}

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
/// The flow is [`apply_pre_execution`](Self::apply_pre_execution) (block-boundary system calls),
/// then [`execute_transaction`](Self::execute_transaction) per transaction (running it through
/// the registry, building its receipt with running cumulative gas, and committing its state into
/// a [`BlockStateAccumulator`]), then [`finish`](Self::finish). The Canyon create2-deployer and
/// Cobalt system-account transition hooks, the EIP-8130 path, and the Jovian DA-footprint checks
/// are layered on in follow-up work.
#[derive(Debug)]
pub struct BaseBlockExecutor<'a> {
    evm: Evm<'a, BaseEvmTypes>,
    ctx: BaseBlockExecutionCtx,
    block_state: BlockStateAccumulator,
    receipts: Vec<BaseReceiptEnvelope>,
    gas_used: u64,
}

impl<'a> BaseBlockExecutor<'a> {
    /// Creates a block executor over `evm` with the given block-boundary context.
    pub fn new(evm: Evm<'a, BaseEvmTypes>, ctx: BaseBlockExecutionCtx) -> Self {
        Self {
            evm,
            ctx,
            block_state: BlockStateAccumulator::new(),
            receipts: Vec::new(),
            gas_used: 0,
        }
    }

    /// Applies the block-boundary pre-execution system calls, in the reference order: the
    /// EIP-2935 block-hashes call (Prague onwards) then the EIP-4788 beacon-roots call (Cancun
    /// onwards). Each is a system call whose state changes are committed into the block state; if
    /// the target system contract is not deployed, the call is a no-op.
    pub fn apply_pre_execution(&mut self) -> HandlerResult<()> {
        let spec = self.evm.spec_id();
        if (spec as u8) >= (SpecId::PRAGUE as u8) {
            let data = Bytes::copy_from_slice(self.ctx.parent_hash.as_slice());
            let executed = self.evm.system_call(SystemTx::new(HISTORY_STORAGE_ADDRESS, data))?;
            let _ = executed.commit_to(&mut self.block_state);
        }
        if (spec as u8) >= (SpecId::CANCUN as u8)
            && let Some(root) = self.ctx.parent_beacon_block_root
        {
            let data = Bytes::copy_from_slice(root.as_slice());
            let executed = self.evm.system_call(SystemTx::new(BEACON_ROOTS_ADDRESS, data))?;
            let _ = executed.commit_to(&mut self.block_state);
        }
        Ok(())
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

        // Reject a transaction whose gas limit exceeds the block's remaining gas, before executing
        // it. Pre-Regolith deposits are exempt (matching the reference's `is_regolith || !is_deposit`
        // guard); every other transaction — including post-Regolith deposits — is checked.
        let gas_limit = tx.gas_limit();
        let block_gas_limit = self.evm.block().gas_limit.saturating_to::<u64>();
        let block_available_gas = block_gas_limit.saturating_sub(self.gas_used);
        let is_regolith =
            (self.evm.config_spec_id().upgrade() as u8) >= (BaseUpgrade::Regolith as u8);
        if gas_limit > block_available_gas && (is_regolith || !is_deposit) {
            return Err(HandlerError::external(BlockGasLimitExceeded {
                transaction_gas_limit: gas_limit,
                block_available_gas,
            }));
        }

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
