//! Base block executor for EVM2.

use alloy_consensus::{Eip658Value, Receipt, transaction::Recovered};
use alloy_eips::eip2718::Typed2718;
use alloy_primitives::{Address, B256, Bytes, U256, address};
use base_common_consensus::{BaseReceiptEnvelope, DepositReceipt};
use base_common_genesis::BaseFeature;
use base_common_l1_fees::L1FeeParams;
use evm2::{
    BlockStateAccumulator, Evm, SpecId,
    evm::DynDatabase,
    EvmConfigSelector, env::BlockEnv,
    evm::{BEACON_ROOTS_ADDRESS, HISTORY_STORAGE_ADDRESS, SystemTx},
    registry::{HandlerError, HandlerResult},
};

use crate::{
    BaseEvmTypes, BaseForkActivations, BaseTime, Canyon, ChainSpec, Everest, IntrinsicGas,
    L1BlockInfo, transaction::BaseTxEnvelope,
};

/// The caller of system calls (EIP-4788, EIP-2935).
const SYSTEM_ADDRESS: Address = address!("0xfffffffffffffffffffffffffffffffffffffffe");

/// The header fields of a block that its execution depends on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlockEnvInput {
    /// Block number.
    pub number: u64,
    /// Block timestamp in seconds.
    pub timestamp: u64,
    /// Fee recipient.
    pub coinbase: Address,
    /// `prevRandao` (the header's mix hash).
    pub prev_randao: B256,
    /// Block gas limit.
    pub gas_limit: u64,
    /// Base fee per gas.
    pub base_fee: u64,
    /// Parent block hash.
    pub parent_hash: B256,
    /// Parent beacon block root (from Ecotone).
    pub parent_beacon_block_root: Option<B256>,
}

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
/// reserved gas is the transaction's gas limit, plus — for a Base transaction — the payer's
/// authentication gas ([`IntrinsicGas::payer_auth_cost_of`]), which is metered on top
/// of the sender-signed gas limit (matching the reference `reserved_block_gas`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockGasLimitExceeded {
    /// The transaction's reserved gas (gas limit, plus Base transaction payer-auth reservation).
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

/// Error returned for a Base transaction, whose enshrined execution path this engine does not
/// implement yet.
///
/// The `0x79` receipt commits to the number of call phases that committed, which only the
/// enshrined execution path can produce, so the transaction is rejected before execution rather
/// than included with a receipt that disagrees with the reference executor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TxBaseExecutionUnsupported;

impl core::fmt::Display for TxBaseExecutionUnsupported {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Base transaction execution is not supported by this engine")
    }
}

impl core::error::Error for TxBaseExecutionUnsupported {}

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
/// then [`apply_transition_hooks`](Self::apply_transition_hooks) (the Canyon/Denim/Everest
/// irregular state transitions), then [`execute_transaction`](Self::execute_transaction) per
/// transaction (running it through the registry, building its receipt with running cumulative gas,
/// and committing its state into a [`BlockStateAccumulator`]), then [`finish`](Self::finish). The
/// Base transaction path and the Jovian DA-footprint checks are layered on in follow-up work.
#[derive(Debug)]
pub struct BaseBlockExecutor<'a> {
    evm: Evm<'a, BaseEvmTypes>,
    ctx: BaseBlockExecutionCtx,
    block_state: BlockStateAccumulator,
    receipts: Vec<BaseReceiptEnvelope>,
    gas_used: u64,
    da_footprint_used: u64,
    /// Whether the block's L1 fee parameters have been read (see [`L1BlockInfo`]).
    l1_loaded: bool,
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
            l1_loaded: false,
        }
    }

    /// Creates the EVM for a block and a block executor over it, with the block's
    /// pre-execution system calls and upgrade transitions applied.
    pub fn for_block(
        chain: &ChainSpec,
        env: &BlockEnvInput,
        database: impl DynDatabase + 'a,
    ) -> HandlerResult<Self> {
        let spec = chain.spec_at(env.timestamp);
        let block = BlockEnv::<BaseEvmTypes> {
            number: U256::from(env.number),
            beneficiary: env.coinbase,
            timestamp: U256::from(env.timestamp),
            gas_limit: U256::from(env.gas_limit),
            basefee: U256::from(env.base_fee),
            prevrandao: env.prev_randao.into(),
            ..BlockEnv::<BaseEvmTypes>::default()
        };
        let config =
            <evm2::BaseEvmConfigSelector as EvmConfigSelector<BaseEvmTypes>>::execution_config(spec);
        let mut version = *config.version();
        version.chain_id = chain.chain_id;
        let evm = Evm::new_with_execution_config(
            config.with_version(version),
            spec,
            block,
            BaseEvmTypes::tx_registry(spec),
            database,
            BaseEvmTypes::precompiles(spec),
        );
        let ctx = BaseBlockExecutionCtx {
            parent_hash: env.parent_hash,
            parent_beacon_block_root: env.parent_beacon_block_root,
        };
        let mut executor = Self::new(evm, ctx);
        executor.apply_pre_execution()?;
        executor.apply_transition_hooks(chain)?;
        Ok(executor)
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
        self.evm.ext_mut().tx_origin = SYSTEM_ADDRESS;
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
    /// Canyon create2-deployer force-deploy, the Denim `BaseTime` predeploy install, then the
    /// Everest Base transaction system-account stub. Each is fork-gated on `chain_spec` at this
    /// block's timestamp and commits its state into the block state. Must run after
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
        Everest::ensure_base_tx_system_accounts(
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

        // Reject a transaction whose reserved gas exceeds the block's remaining gas, before
        // executing it. For a Base transaction the reservation adds the payer's
        // authentication gas (metered on top of the sender-signed gas limit), matching the
        // reference `reserved_block_gas`. Pre-Regolith deposits are exempt (matching the reference's
        // `is_regolith || !is_deposit` guard); every other transaction is checked.
        let mut reserved_gas = tx.gas_limit();
        if let Some(signed) = tx.as_base_tx() {
            reserved_gas = reserved_gas.saturating_add(IntrinsicGas::payer_auth_cost_of(signed));
        }
        let block_gas_limit = self.evm.block().gas_limit.saturating_to::<u64>();
        let block_available_gas = block_gas_limit.saturating_sub(self.gas_used);
        let deposit_gas_accounting =
            self.evm.config_spec_id().is_feature_enabled(BaseFeature::DepositGasAccounting);
        if reserved_gas > block_available_gas && (deposit_gas_accounting || !is_deposit) {
            return Err(HandlerError::external(BlockGasLimitExceeded {
                transaction_gas_limit: reserved_gas,
                block_available_gas,
            }));
        }

        // From Jovian, meter each non-deposit transaction's DA footprint against the block's DA
        // footprint budget (the block gas limit less what earlier transactions consumed). Deposits
        // are exempt. Accumulated below into `da_footprint_used` (surfaced as `blob_gas_used`).
        let tx_da_footprint = if self
            .evm
            .config_spec_id()
            .is_feature_enabled(BaseFeature::DaFootprint)
            && !is_deposit
        {
            let enveloped = tx.enveloped().map(|bytes| bytes.as_ref()).unwrap_or_default();
            let scalar = L1BlockInfo::da_footprint_gas_scalar(&mut self.evm)?;
            let footprint = L1FeeParams {
                da_footprint_gas_scalar: Some(U256::from(scalar)),
                ..L1FeeParams::default()
            }
            .jovian_da_footprint(enveloped);
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

        if tx.as_base_tx().is_some() {
            return Err(HandlerError::external(TxBaseExecutionUnsupported));
        }

        // The L1 fee parameters are read once per block, before the first non-deposit
        // transaction runs, from the state the deposits before it left.
        if !is_deposit && !self.l1_loaded {
            let spec = self.evm.config_spec_id();
            self.evm.ext_mut().l1 = L1BlockInfo::fetch(&mut self.evm, spec)?;
            self.l1_loaded = true;
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
            let deposit_receipt_version = self
                .evm
                .config_spec_id()
                .is_feature_enabled(BaseFeature::DepositReceiptVersion)
                .then_some(1);
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
