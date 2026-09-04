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

use crate::{
    BaseEvmTypes, BaseForkActivations, BaseTime, Canyon, Cobalt, transaction::BaseTxEnvelope,
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

/// Block-boundary context for pre-execution system calls.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BaseBlockExecutionCtx {
    /// The parent block hash, stored by the EIP-2935 block-hashes contract (Prague onwards).
    pub parent_hash: B256,
    /// The parent beacon block root, stored by the EIP-4788 beacon-roots contract (Cancun
    /// onwards). Required for every post-Cancun non-genesis block; `None` is only valid before
    /// Cancun. A post-Cancun `None` is rejected by [`BaseBlockExecutor::apply_pre_execution`].
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
    /// The block's accumulated Jovian DA footprint (zero before Jovian). Surfaced in the block
    /// result as `blob_gas_used`, mirroring the reference.
    pub blob_gas_used: u64,
}

/// Executes a Base block's transactions on an EVM2 [`Evm`], building receipts and accumulating
/// the block's state delta.
///
/// The flow is [`apply_pre_execution`](Self::apply_pre_execution) (block-boundary system calls),
/// then [`apply_transition_hooks`](Self::apply_transition_hooks) (the Canyon/Cobalt
/// irregular state transitions), then [`execute_transaction`](Self::execute_transaction) per
/// transaction (running it through the registry, building its receipt with running cumulative gas,
/// and committing its state into a [`BlockStateAccumulator`]), then [`finish`](Self::finish). The
/// EIP-8130 path and the Jovian DA-footprint checks are layered on in follow-up work.
#[derive(Debug)]
pub struct BaseBlockExecutor<'a> {
    evm: Evm<'a, BaseEvmTypes>,
    ctx: BaseBlockExecutionCtx,
    block_state: BlockStateAccumulator,
    receipts: Vec<BaseReceiptEnvelope>,
    gas_used: u64,
    da_footprint_used: u64,
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
            da_footprint_used: 0,
        }
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
        // The genesis block never runs the pre-execution system calls, matching the reference.
        let is_genesis = self.evm.block().number.is_zero();

        // EIP-2935 block-hashes call (Prague onwards), skipped at genesis.
        if (spec as u8) >= (SpecId::PRAGUE as u8) && !is_genesis {
            let data = Bytes::copy_from_slice(self.ctx.parent_hash.as_slice());
            let executed = self.evm.system_call(SystemTx::new(HISTORY_STORAGE_ADDRESS, data))?;
            let _ = executed.commit_to(&mut self.block_state);
        }

        // EIP-4788 beacon-roots call (Cancun onwards). A post-Cancun block must carry a parent
        // beacon block root; at genesis that root must be zero and no system call runs.
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
                let data = Bytes::copy_from_slice(root.as_slice());
                let executed = self.evm.system_call(SystemTx::new(BEACON_ROOTS_ADDRESS, data))?;
                let _ = executed.commit_to(&mut self.block_state);
            }
        }
        Ok(())
    }

    /// Applies the Base transition-block irregular state changes, in the reference order: the
    /// Canyon create2-deployer force-deploy, the Cobalt `BaseTime` predeploy install, then the
    /// Cobalt EIP-8130 system-account stub. Each is fork-gated on `chain_spec` at this block's
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
        Cobalt::ensure_eip8130_system_accounts(
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

        // From Jovian, meter each non-deposit transaction's DA footprint against the block's DA
        // footprint budget (the block gas limit less what earlier transactions consumed). Deposits
        // are exempt. Accumulated below into `da_footprint_used` (surfaced as `blob_gas_used`).
        let is_jovian = (self.evm.config_spec_id().upgrade() as u8) >= (BaseUpgrade::Jovian as u8);
        let tx_da_footprint = if is_jovian && !is_deposit {
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
        // later receipt's cumulative gas. Practically unreachable given the block-gas pre-check
        // above, but kept as a hard guard against a corrupted cumulative total.
        self.gas_used = self
            .gas_used
            .checked_add(result.tx_gas_used())
            .ok_or_else(|| HandlerError::external(CumulativeGasOverflow))?;
        let cumulative_gas_used = self.gas_used;
        // Accumulate the DA footprint metered above (zero for deposits and before Jovian).
        self.da_footprint_used = self.da_footprint_used.saturating_add(tx_da_footprint);
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
        let result = BlockExecutionResult {
            receipts: self.receipts,
            gas_used: self.gas_used,
            blob_gas_used: self.da_footprint_used,
        };
        (self.evm, result, self.block_state)
    }
}
