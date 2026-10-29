//! Base block executor for EVM2.

use alloy_consensus::{Eip658Value, Receipt, Transaction, transaction::Recovered};
use alloy_eips::eip2718::Typed2718;
use alloy_primitives::{Address, B256, Bytes};
use base_common_consensus::{BaseReceipt, DepositReceipt};
use base_common_genesis::BaseUpgrade;
use evm2::{
    BlockStateAccumulator, Evm, SpecId, TxResultWithState,
    evm::{BEACON_ROOTS_ADDRESS, HISTORY_STORAGE_ADDRESS, SystemTx},
    registry::{HandlerError, HandlerResult},
};

use crate::{
    BaseBlockState, BaseEvmTypes, BaseForkActivations, BaseTime, Canyon, Everest, IntrinsicGas,
    L1Block, transaction::BaseTxEnvelope,
};

/// Error returned when a block's cumulative gas used would overflow `u64`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CumulativeGasOverflow;

impl core::fmt::Display for CumulativeGasOverflow {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("block cumulative gas used overflowed u64")
    }
}

impl core::error::Error for CumulativeGasOverflow {}

/// Error returned when a block's pre-execution block-boundary state is invalid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreExecutionError {
    /// A post-Cancun block did not carry a parent beacon block root. EIP-4788 requires every
    /// post-Cancun block to supply one, so its absence makes the block invalid.
    MissingParentBeaconBlockRoot,
    /// A Cancun genesis block (number 0) carried a non-zero parent beacon block root. EIP-4788
    /// requires the genesis beacon root to be zero.
    CancunGenesisParentBeaconBlockRootNotZero {
        /// The non-zero parent beacon block root the genesis block carried.
        parent_beacon_block_root: B256,
    },
}

impl core::fmt::Display for PreExecutionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::MissingParentBeaconBlockRoot => {
                f.write_str("missing parent beacon block root for post-Cancun block")
            }
            Self::CancunGenesisParentBeaconBlockRootNotZero { parent_beacon_block_root } => write!(
                f,
                "Cancun genesis parent beacon block root must be zero, got {parent_beacon_block_root}"
            ),
        }
    }
}

impl core::error::Error for PreExecutionError {}

/// Error returned when a transaction's reserved gas exceeds the block's remaining gas.
///
/// Mirrors the reference `BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas`: a
/// transaction whose reserved gas is larger than the block's unused gas cannot be included. The
/// reserved gas is the transaction's gas limit, plus — for an EIP-8130 transaction — the payer's
/// worst-case authentication gas ([`IntrinsicGas::max_payer_auth_cost`]), which is metered on top
/// of the sender-signed gas limit (matching the reference `reserved_block_gas`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockGasLimitExceeded {
    /// The transaction's reserved gas (gas limit, plus EIP-8130 payer-auth reservation).
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

/// Error returned when a transaction's Jovian DA footprint exceeds the block's remaining DA
/// footprint budget.
///
/// Mirrors the reference `BaseBlockExecutionError::TransactionDaFootprintAboveGasLimit`: from
/// Jovian, each non-deposit transaction's DA footprint (its FastLZ-estimated compressed size times
/// the DA-footprint gas scalar) is metered against the block gas limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DaFootprintAboveGasLimit {
    /// The transaction's DA footprint.
    pub transaction_da_footprint: u64,
    /// The block's remaining DA footprint budget (block gas limit minus accumulated DA footprint).
    pub available_block_da_footprint: u64,
}

impl core::fmt::Display for DaFootprintAboveGasLimit {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "transaction DA footprint {} is more than the block's available DA footprint {}",
            self.transaction_da_footprint, self.available_block_da_footprint
        )
    }
}

impl core::error::Error for DaFootprintAboveGasLimit {}

/// Error returned for an EIP-8130 transaction, whose enshrined execution path this engine does not
/// implement yet.
///
/// The `0x79` receipt commits to the number of call phases that committed, which only the
/// enshrined execution path can produce, so the transaction is rejected before execution rather
/// than included with a receipt that disagrees with the reference executor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Eip8130ExecutionUnsupported;

impl core::fmt::Display for Eip8130ExecutionUnsupported {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("EIP-8130 transaction execution is not supported by this engine")
    }
}

impl core::error::Error for Eip8130ExecutionUnsupported {}

/// Block context that is not part of the EVM block environment: the inputs of the pre-execution
/// system calls and the header fields block assembly copies through.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BaseBlockExecutionCtx {
    /// The parent block hash, stored by the EIP-2935 block-hashes contract (Prague onwards).
    pub parent_hash: B256,
    /// The parent beacon block root, stored by the EIP-4788 beacon-roots contract (Cancun
    /// onwards). Required for every post-Cancun non-genesis block; `None` is only valid before
    /// Cancun. A post-Cancun `None` is rejected by [`BaseBlockExecutor::apply_pre_execution`].
    pub parent_beacon_block_root: Option<B256>,
    /// The block's extra data, which encodes the Holocene EIP-1559 parameters (and the Jovian
    /// minimum base fee). Execution ignores it; block assembly writes it into the header.
    pub extra_data: Bytes,
}

/// The outcome of executing a block's transactions: one receipt per transaction (in order) and
/// the total gas used.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlockExecutionResult {
    /// The transaction receipts, in execution order.
    pub receipts: Vec<BaseReceipt>,
    /// The block's cumulative gas used (the last receipt's cumulative gas, or zero).
    pub gas_used: u64,
    /// The block's accumulated Jovian DA footprint (zero before Jovian). Surfaced in the block
    /// result as `blob_gas_used`, mirroring the reference.
    pub blob_gas_used: u64,
}

/// An executed transaction whose state changes are detached and not yet committed to the block.
///
/// Produced by [`BaseBlockExecutor::execute_transaction_without_commit`] and consumed by
/// [`BaseBlockExecutor::commit_transaction`].
#[derive(Clone, Debug)]
pub struct BaseExecutedTx {
    /// The execution result and its detached state changes.
    pub result: TxResultWithState<BaseEvmTypes>,
    /// The EIP-2718 type byte of the executed transaction.
    pub ty: u8,
    /// The depositor's nonce before a post-Regolith deposit executed; `None` for every other
    /// transaction. Recorded in the deposit receipt.
    pub deposit_nonce: Option<u64>,
    /// The transaction's Jovian DA footprint (zero for deposits and before Jovian).
    pub da_footprint: u64,
    /// Whether the transaction is a deposit.
    pub is_deposit: bool,
}

/// Executes a Base block's transactions on an EVM2 [`Evm`], building receipts and committing
/// every state change through a [`BaseBlockState`].
///
/// The flow is [`apply_pre_execution`](Self::apply_pre_execution) (block-boundary system calls),
/// then [`apply_transition_hooks`](Self::apply_transition_hooks) (the Canyon/Denim/Everest
/// irregular state transitions), then [`execute_transaction`](Self::execute_transaction) per
/// transaction, then [`finish`](Self::finish). A transaction can also be executed and committed in
/// two steps ([`execute_transaction_without_commit`](Self::execute_transaction_without_commit) and
/// [`commit_transaction`](Self::commit_transaction)), which is how Reth drives block execution.
#[derive(Debug)]
pub struct BaseBlockExecutor<'a, S = BlockStateAccumulator> {
    evm: Evm<'a, BaseEvmTypes>,
    ctx: BaseBlockExecutionCtx,
    block_state: S,
    receipts: Vec<BaseReceipt>,
    gas_used: u64,
    da_footprint_used: u64,
}

impl<'a> BaseBlockExecutor<'a> {
    /// Creates a block executor over `evm` that accumulates the block's state delta in a
    /// [`BlockStateAccumulator`].
    pub fn new(evm: Evm<'a, BaseEvmTypes>, ctx: BaseBlockExecutionCtx) -> Self {
        Self::with_block_state(evm, ctx, BlockStateAccumulator::new())
    }
}

impl<'a, S: BaseBlockState> BaseBlockExecutor<'a, S> {
    /// Creates a block executor over `evm` that commits state through `block_state`.
    pub const fn with_block_state(
        evm: Evm<'a, BaseEvmTypes>,
        ctx: BaseBlockExecutionCtx,
        block_state: S,
    ) -> Self {
        Self { evm, ctx, block_state, receipts: Vec::new(), gas_used: 0, da_footprint_used: 0 }
    }

    /// Applies the block-boundary pre-execution system calls, in the reference order: the
    /// EIP-2935 block-hashes call (Prague onwards) then the EIP-4788 beacon-roots call (Cancun
    /// onwards). Each is a system call whose state changes are committed into the block state; if
    /// the target system contract is not deployed, the call is a no-op.
    ///
    /// The genesis block (number 0) runs neither call, matching the reference: EIP-2935 no-ops at
    /// genesis, and EIP-4788 requires the genesis beacon root to be zero and performs no system
    /// call. A post-Cancun block that carries no parent beacon block root, or a Cancun genesis
    /// block whose beacon root is non-zero, is rejected as invalid ([`PreExecutionError`]).
    pub fn apply_pre_execution(&mut self) -> HandlerResult<()> {
        let spec = self.evm.spec_id();
        let is_genesis = self.evm.block().number.is_zero();

        if (spec as u8) >= (SpecId::PRAGUE as u8) && !is_genesis {
            let data = Bytes::copy_from_slice(self.ctx.parent_hash.as_slice());
            self.system_call(HISTORY_STORAGE_ADDRESS, data)?;
        }

        if (spec as u8) >= (SpecId::CANCUN as u8) {
            let root = self.ctx.parent_beacon_block_root.ok_or_else(|| {
                HandlerError::external(PreExecutionError::MissingParentBeaconBlockRoot)
            })?;
            if is_genesis {
                if !root.is_zero() {
                    return Err(HandlerError::external(
                        PreExecutionError::CancunGenesisParentBeaconBlockRootNotZero {
                            parent_beacon_block_root: root,
                        },
                    ));
                }
            } else {
                self.system_call(BEACON_ROOTS_ADDRESS, Bytes::copy_from_slice(root.as_slice()))?;
            }
        }
        Ok(())
    }

    fn system_call(&mut self, address: Address, data: Bytes) -> HandlerResult<()> {
        let state = self.evm.system_call(SystemTx::new(address, data))?.detach().pending_state;
        self.block_state.commit(&mut self.evm, state);
        Ok(())
    }

    /// Applies the Base transition-block irregular state changes, in the reference order: the
    /// Canyon create2-deployer force-deploy, the Denim `BaseTime` predeploy install, then the
    /// Everest EIP-8130 system-account stub. Each is fork-gated on `chain_spec` at this block's
    /// timestamp and commits its state into the block state. Must run after
    /// [`apply_pre_execution`](Self::apply_pre_execution) and before any transactions.
    pub fn apply_transition_hooks(
        &mut self,
        chain_spec: &impl BaseForkActivations,
    ) -> HandlerResult<()> {
        let timestamp = self.evm.block().timestamp.saturating_to::<u64>();
        Canyon::ensure_create2_deployer(
            chain_spec,
            timestamp,
            &mut self.evm,
            &mut self.block_state,
        )?;
        BaseTime::ensure_predeploy(chain_spec, timestamp, &mut self.evm, &mut self.block_state)?;
        Everest::ensure_eip8130_system_accounts(
            chain_spec,
            timestamp,
            &mut self.evm,
            &mut self.block_state,
        )?;
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

    /// Returns the block state that committed changes are recorded in.
    pub const fn block_state_mut(&mut self) -> &mut S {
        &mut self.block_state
    }

    /// Returns the receipts of the transactions committed so far.
    pub fn receipts(&self) -> &[BaseReceipt] {
        &self.receipts
    }

    /// Returns the block's cumulative gas used so far.
    pub const fn gas_used(&self) -> u64 {
        self.gas_used
    }

    /// Executes `tx`, appending its receipt (with running cumulative gas) and committing its
    /// state changes into the block state. Returns the gas the transaction used.
    pub fn execute_transaction(&mut self, tx: &Recovered<BaseTxEnvelope>) -> HandlerResult<u64> {
        let executed = self.execute_transaction_without_commit(tx)?;
        self.commit_transaction(executed)
    }

    /// Checks `tx` against the block's remaining gas and DA footprint, then executes it without
    /// committing its state changes.
    pub fn execute_transaction_without_commit(
        &mut self,
        tx: &Recovered<BaseTxEnvelope>,
    ) -> HandlerResult<BaseExecutedTx> {
        let ty = tx.ty();
        let is_deposit = tx.is_deposit();
        let upgrade = self.evm.config_spec_id().upgrade();
        let at = |fork: BaseUpgrade| (upgrade as u8) >= (fork as u8);

        // A transaction's reserved gas must fit in the block's remaining gas. For an EIP-8130
        // transaction the reservation adds the payer's worst-case authentication gas, metered on
        // top of the sender-signed gas limit (the reference `reserved_block_gas`). Pre-Regolith
        // deposits are exempt (the reference's `is_regolith || !is_deposit` guard).
        let mut reserved_gas = tx.gas_limit();
        if let Some(signed) = tx.as_eip8130() {
            let payer_auth =
                IntrinsicGas::max_payer_auth_cost(signed).map_err(HandlerError::external)?;
            reserved_gas = reserved_gas.saturating_add(payer_auth);
        }
        let block_gas_limit = self.evm.block().gas_limit.saturating_to::<u64>();
        let block_available_gas = block_gas_limit.saturating_sub(self.gas_used);
        if reserved_gas > block_available_gas && (at(BaseUpgrade::Regolith) || !is_deposit) {
            return Err(HandlerError::external(BlockGasLimitExceeded {
                transaction_gas_limit: reserved_gas,
                block_available_gas,
            }));
        }

        // From Jovian, each non-deposit transaction's DA footprint is metered against the block's
        // DA footprint budget: the block gas limit less what earlier transactions consumed.
        let da_footprint = if at(BaseUpgrade::Jovian) && !is_deposit {
            L1Block::ensure_fee_params(&mut self.evm)?;
            let enveloped = tx.enveloped().map(|bytes| bytes.as_ref()).unwrap_or_default();
            let footprint = self.evm.block().ext.jovian_da_footprint(enveloped);
            let available = block_gas_limit.saturating_sub(self.da_footprint_used);
            if footprint > available {
                return Err(HandlerError::external(DaFootprintAboveGasLimit {
                    transaction_da_footprint: footprint,
                    available_block_da_footprint: available,
                }));
            }
            footprint
        } else {
            0
        };

        if tx.as_eip8130().is_some() {
            return Err(HandlerError::external(Eip8130ExecutionUnsupported));
        }

        // The deposit receipt records the depositor's nonce before the deposit executes, read
        // untracked so it does not perturb execution. Only post-Regolith deposit receipts carry it.
        let deposit_nonce = if is_deposit && at(BaseUpgrade::Regolith) {
            let nonce = self
                .evm
                .state_mut()
                .account_info_untracked(&tx.signer())?
                .map(|info| info.nonce)
                .unwrap_or_default();
            Some(nonce)
        } else {
            None
        };

        let result = self.evm.transact(tx)?.detach();
        Ok(BaseExecutedTx { result, ty, deposit_nonce, da_footprint, is_deposit })
    }

    /// Commits `executed`'s state changes into the block state and appends its receipt, with the
    /// running cumulative gas. Returns the gas the transaction used.
    pub fn commit_transaction(&mut self, executed: BaseExecutedTx) -> HandlerResult<u64> {
        let TxResultWithState { result, pending_state, .. } = executed.result;
        let tx_gas_used = result.tx_gas_used();
        // Fail on overflow rather than silently saturating, which would corrupt this and every
        // later receipt's cumulative gas.
        self.gas_used = self
            .gas_used
            .checked_add(tx_gas_used)
            .ok_or_else(|| HandlerError::external(CumulativeGasOverflow))?;
        self.da_footprint_used = self.da_footprint_used.saturating_add(executed.da_footprint);
        self.block_state.commit(&mut self.evm, pending_state);

        let receipt = Receipt {
            status: Eip658Value::Eip658(result.status),
            cumulative_gas_used: self.gas_used,
            logs: result.logs,
        };
        let receipt = if executed.is_deposit {
            let canyon = (self.evm.config_spec_id().upgrade() as u8) >= (BaseUpgrade::Canyon as u8);
            BaseReceipt::Deposit(DepositReceipt {
                inner: receipt,
                deposit_nonce: executed.deposit_nonce,
                deposit_receipt_version: canyon.then_some(1),
            })
        } else {
            match executed.ty {
                1 => BaseReceipt::Eip2930(receipt),
                2 => BaseReceipt::Eip1559(receipt),
                4 => BaseReceipt::Eip7702(receipt),
                // These type bytes mirror the handlers registered in `tx_registry()`; the assert
                // flags a registered type that gains a handler but not a receipt arm here.
                other => {
                    debug_assert_eq!(other, 0, "unmapped standard tx type {other:#x} in receipts");
                    BaseReceipt::Legacy(receipt)
                }
            }
        };
        self.receipts.push(receipt);
        Ok(tx_gas_used)
    }

    /// Finalizes the block, returning the EVM, the execution result (receipts + gas used), and
    /// the block state.
    pub fn finish(self) -> (Evm<'a, BaseEvmTypes>, BlockExecutionResult, S) {
        let result = BlockExecutionResult {
            receipts: self.receipts,
            gas_used: self.gas_used,
            blob_gas_used: self.da_footprint_used,
        };
        (self.evm, result, self.block_state)
    }
}
