//! Handler related to Base chain
use alloc::{boxed::Box, vec::Vec};

use base_common_consensus::Predeploys;
use base_common_genesis::BaseUpgrade;
use revm::{
    context::{
        LocalContextTr,
        journaled_state::{JournalCheckpoint, account::JournaledAccountTr},
        result::InvalidTransaction,
    },
    context_interface::{
        Block, Cfg, ContextTr, JournalTr, Transaction,
        cfg::gas::InitialAndFloorGas,
        context::take_error,
        result::{EVMError, ExecutionResult, FromStringError, ResultGas},
    },
    handler::{
        EthFrame, EvmTr, FrameResult, Handler, MainnetHandler,
        evm::FrameTr,
        handle_reservoir_remaining_gas,
        handler::EvmTrError,
        post_execution::{self, reimburse_caller},
        pre_execution::{calculate_caller_fee, validate_account_nonce_and_code_with_components},
    },
    inspector::{Inspector, InspectorEvmTr, InspectorHandler},
    interpreter::{GasTracker, interpreter::EthInterpreter, interpreter_action::FrameInit},
    primitives::U256,
};

use crate::{
    BaseContextTr, BaseHaltReason, L1BlockInfo,
    transaction::{BaseTransactionError, BaseTxTr, DEPOSIT_TRANSACTION_TYPE},
};

/// Base handler extends the [`Handler`] with Base-specific logic.
#[derive(Debug, Clone)]
pub struct BaseHandler<EVM, ERROR, FRAME> {
    /// Mainnet handler allows us to use functions from the mainnet handler inside the Base handler.
    /// So we dont duplicate the logic
    pub mainnet: MainnetHandler<EVM, ERROR, FRAME>,
}

impl<EVM, ERROR, FRAME> BaseHandler<EVM, ERROR, FRAME> {
    /// Create a new Base handler.
    pub fn new() -> Self {
        Self { mainnet: MainnetHandler::default() }
    }
}

impl<EVM, ERROR, FRAME> Default for BaseHandler<EVM, ERROR, FRAME> {
    fn default() -> Self {
        Self::new()
    }
}

/// Trait to check if the error is a transaction error.
///
/// Used in `cache_error` handler to catch deposit transaction that was halted.
pub trait IsTxError {
    /// Check if the error is a transaction error.
    fn is_tx_error(&self) -> bool;
}

impl<DB, TX> IsTxError for EVMError<DB, TX> {
    fn is_tx_error(&self) -> bool {
        matches!(self, Self::Transaction(_))
    }
}

impl<EVM, ERROR, FRAME> Handler for BaseHandler<EVM, ERROR, FRAME>
where
    EVM: EvmTr<Context: BaseContextTr, Frame = FRAME>,
    ERROR: EvmTrError<EVM> + From<BaseTransactionError> + FromStringError + IsTxError,
    FRAME: FrameTr<FrameResult = FrameResult, FrameInit = FrameInit>,
{
    type Evm = EVM;
    type Error = ERROR;
    type HaltReason = BaseHaltReason;

    fn validate_env(&self, evm: &mut Self::Evm) -> Result<(), Self::Error> {
        // Do not perform any extra validation for deposit transactions, they are pre-verified on L1.
        let ctx = evm.ctx();
        let tx = ctx.tx();
        let tx_type = tx.tx_type();
        if tx_type == DEPOSIT_TRANSACTION_TYPE {
            // Do not allow for a system transaction to be processed if Regolith is enabled.
            if tx.is_system_transaction()
                && evm.ctx().cfg().spec().is_enabled_in(BaseUpgrade::Regolith)
            {
                return Err(BaseTransactionError::DepositSystemTxPostRegolith.into());
            }
            return Ok(());
        }

        // Check that non-deposit transactions have enveloped_tx set
        if tx.enveloped_tx().is_none() {
            return Err(BaseTransactionError::MissingEnvelopedTx.into());
        }

        self.mainnet.validate_env(evm)
    }

    fn validate_against_state_and_deduct_caller(
        &self,
        evm: &mut Self::Evm,
        _initial_and_floor_gas: &mut InitialAndFloorGas,
    ) -> Result<(), Self::Error> {
        let (block, tx, cfg, journal, chain, _) = evm.ctx().all_mut();
        let spec = cfg.spec();

        if tx.tx_type() == DEPOSIT_TRANSACTION_TYPE {
            let basefee = block.basefee() as u128;
            let blob_price = block.blob_gasprice().unwrap_or_default();
            // deposit skips max fee check and just deducts the effective balance spending.

            let mut caller = journal.load_account_with_code_mut(tx.caller())?.data;

            let effective_balance_spending = tx
                .effective_balance_spending(basefee, blob_price)
                .expect("Deposit transaction effective balance spending overflow")
                - tx.value();

            // Mind value should be added first before subtracting the effective balance spending.
            let mut new_balance = caller
                .balance()
                .saturating_add(U256::from(tx.mint().unwrap_or_default()))
                .saturating_sub(effective_balance_spending);

            if cfg.is_balance_check_disabled() {
                // Make sure the caller's balance is at least the value of the transaction.
                // this is not consensus critical, and it is used in testing.
                new_balance = new_balance.max(tx.value());
            }

            // set the new balance and bump the nonce if it is a call
            caller.set_balance(new_balance);
            if tx.kind().is_call() {
                caller.bump_nonce();
            }

            return Ok(());
        }

        // L1 block info is stored in the context for later use.
        // and it will be reloaded from the database if it is not for the current block.
        if chain.l2_block != Some(block.number()) {
            *chain = L1BlockInfo::try_fetch(journal.db_mut(), block.number(), spec)?;
        }

        let mut caller_account = journal.load_account_with_code_mut(tx.caller())?.data;

        // validates account nonce and code
        validate_account_nonce_and_code_with_components(&caller_account.account().info, tx, cfg)?;

        // check additional cost and deduct it from the caller's balances
        let mut balance = caller_account.account().info.balance;

        if !cfg.is_fee_charge_disabled() {
            let Some(additional_cost) = chain.tx_cost_with_tx(tx, spec) else {
                return Err(ERROR::from_string(
                    "[OPTIMISM] Failed to load enveloped transaction.".into(),
                ));
            };
            let Some(new_balance) = balance.checked_sub(additional_cost) else {
                return Err(InvalidTransaction::LackOfFundForMaxFee {
                    fee: Box::new(additional_cost),
                    balance: Box::new(balance),
                }
                .into());
            };
            balance = new_balance
        }

        let balance = calculate_caller_fee(balance, tx, block, cfg)?;

        // make changes to the account
        caller_account.set_balance(balance);
        if tx.kind().is_call() {
            caller_account.bump_nonce();
        }

        Ok(())
    }

    fn last_frame_result(
        &mut self,
        evm: &mut Self::Evm,
        frame_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
        parent_gas: &mut GasTracker,
    ) -> Result<(), Self::Error> {
        // Base: this used to be customized for pre-Regolith deposits, but since
        // Base has always had regolith active, this now matches the revm source
        // exactly.
        let instruction_result = frame_result.instruction_result();

        // All regular gas was forwarded to the first frame: consume it on the
        // transaction-level gas; the settle below returns the frame's unused
        // part.
        parent_gas.spend_all();

        // Settle the frame into the transaction-level gas like a parent frame.
        handle_reservoir_remaining_gas(
            instruction_result,
            parent_gas,
            frame_result.gas_mut().tracker_mut(),
        );

        // Refund the EIP-2780 refundable first-frame charge when no account
        // leaf was created, exactly like `EthFrame::return_result` refunds
        // the upfront CALL/CREATE state charges of inner frames.
        if let Some(charge) = frame_result.refundable_state_gas(evm.ctx().cfg().gas_params()) {
            parent_gas.refill_reservoir(charge);
            // Unlike an inner frame's caller, the transaction ends here: an
            // exceptional halt consumes all regular gas, including the
            // spilled portion the refill just credited back to `remaining`.
            if instruction_result.is_halt() {
                parent_gas.spend_all();
            }
        }

        // The frame result carries the transaction-level gas onward to the
        // post-execution phase.
        *frame_result.gas_mut().tracker_mut() = *parent_gas;

        Ok(())
    }

    fn reimburse_caller(
        &self,
        evm: &mut Self::Evm,
        frame_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        let mut additional_refund = U256::ZERO;

        if evm.ctx().tx().tx_type() != DEPOSIT_TRANSACTION_TYPE
            && !evm.ctx().cfg().is_fee_charge_disabled()
        {
            let spec = evm.ctx().cfg().spec();
            additional_refund = evm.ctx().chain().operator_fee_refund(frame_result.gas(), spec);
        }

        reimburse_caller(evm.ctx(), frame_result.gas(), additional_refund).map_err(From::from)
    }

    fn refund(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
        eip7702_refund: i64,
    ) -> Result<(), Self::Error> {
        // Base: this used to be customized for pre-Regolith deposits, but since
        // Base has always had regolith active, this now matches the revm source
        // exactly.

        post_execution::refund(evm.ctx().cfg().gas_params(), exec_result.gas_mut(), eip7702_refund);

        // Gas limit pricing: a non-deposit transaction is billed and reported for the whole
        // declared gas budget it reserved, so the sequencer's revenue per gas is exactly the
        // tip it was ranked by and padding the limit costs what it reserves. Zeroing the
        // remaining gas and the refund here makes the floor check, the caller reimbursement,
        // the beneficiary reward, and the receipt all observe `gas_used == gas_limit`.
        // Deposits pay no L2 fee and are not ordered by tip, so they keep metered gas.
        // Simulation (`disable_fee_charge`) keeps metered gas so `eth_estimateGas` still
        // finds the gas a call needs.
        let ctx = evm.ctx();
        let cfg = ctx.cfg();
        if cfg.spec().charges_gas_limit()
            && !cfg.is_fee_charge_disabled()
            && ctx.tx().tx_type() != DEPOSIT_TRANSACTION_TYPE
        {
            let gas = exec_result.gas_mut();
            gas.spend_all();
            gas.set_refund(0);
        }

        Ok(())
    }

    fn reward_beneficiary(
        &self,
        evm: &mut Self::Evm,
        frame_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        let is_deposit = evm.ctx().tx().tx_type() == DEPOSIT_TRANSACTION_TYPE;

        // Transfer fee to coinbase/beneficiary.
        if is_deposit {
            return Ok(());
        }

        self.mainnet.reward_beneficiary(evm, frame_result)?;
        let basefee = evm.ctx().block().basefee() as u128;

        let ctx = evm.ctx();
        let enveloped = ctx.tx().enveloped_tx().cloned();
        let spec = ctx.cfg().spec();
        let l1_block_info = ctx.chain_mut();

        let Some(enveloped_tx) = &enveloped else {
            return Err(ERROR::from_string(
                "[OPTIMISM] Failed to load enveloped transaction.".into(),
            ));
        };

        let l1_cost = l1_block_info.calculate_tx_l1_cost(enveloped_tx, spec);
        let operator_fee_cost = if spec.is_enabled_in(BaseUpgrade::Isthmus) {
            l1_block_info.operator_fee_charge(
                enveloped_tx,
                U256::from(frame_result.gas().used()),
                spec,
            )
        } else {
            U256::ZERO
        };
        let base_fee_amount = U256::from(basefee.saturating_mul(frame_result.gas().used() as u128));

        // Send fees to their respective recipients
        for (recipient, amount) in [
            (Predeploys::L1_FEE_VAULT, l1_cost),
            (Predeploys::BASE_FEE_VAULT, base_fee_amount),
            (Predeploys::OPERATOR_FEE_VAULT, operator_fee_cost),
        ] {
            ctx.journal_mut().balance_incr(recipient, amount)?;
        }

        Ok(())
    }

    fn execution_result(
        &mut self,
        evm: &mut Self::Evm,
        result: <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
        result_gas: ResultGas,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        take_error::<Self::Error, _>(evm.ctx().error())?;

        let exec_result = post_execution::output(evm.ctx(), result, result_gas)
            .map_haltreason(BaseHaltReason::Base);

        if exec_result.is_halt() && evm.ctx().tx().tx_type() == DEPOSIT_TRANSACTION_TYPE {
            return Err(ERROR::from(BaseTransactionError::HaltedDepositPostRegolith));
        }

        // commit transaction
        evm.ctx().journal_mut().commit_tx();
        evm.ctx().chain_mut().clear_tx_l1_cost();
        evm.ctx().local_mut().clear();
        evm.frame_stack().clear();

        Ok(exec_result)
    }

    fn catch_error(
        &self,
        evm: &mut Self::Evm,
        error: Self::Error,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        let is_deposit = evm.ctx().tx().tx_type() == DEPOSIT_TRANSACTION_TYPE;
        let is_tx_error = error.is_tx_error();
        let mut output = Err(error);

        // Deposit transaction can't fail so we manually handle it here.
        if is_tx_error && is_deposit {
            let ctx = evm.ctx();
            let tx = ctx.tx();
            let caller = tx.caller();
            let mint = tx.mint();
            let gas_limit = tx.gas_limit();
            let journal = evm.ctx().journal_mut();

            // discard all changes of this transaction
            // Default JournalCheckpoint is the first checkpoint and will wipe all changes.
            journal.checkpoint_revert(JournalCheckpoint::default());

            let mut acc = journal.load_account_mut(caller)?;
            acc.bump_nonce();
            acc.incr_balance(U256::from(mint.unwrap_or_default()));

            drop(acc); // Drop acc to avoid borrow checker issues.

            // We can now commit the changes.
            journal.commit_tx();

            // clear the journal
            output = Ok(ExecutionResult::Halt {
                reason: BaseHaltReason::FailedDeposit,
                gas: ResultGas::new_with_state_gas(gas_limit, 0, 0, 0),
                logs: Vec::new(),
            })
        } else {
            evm.ctx().journal_mut().discard_tx();
        }

        // do the cleanup
        evm.ctx().chain_mut().clear_tx_l1_cost();
        evm.ctx().local_mut().clear();
        evm.frame_stack().clear();

        output
    }
}

impl<EVM, ERROR> InspectorHandler for BaseHandler<EVM, ERROR, EthFrame<EthInterpreter>>
where
    EVM: InspectorEvmTr<
            Context: BaseContextTr,
            Frame = EthFrame<EthInterpreter>,
            Inspector: Inspector<<<Self as Handler>::Evm as EvmTr>::Context, EthInterpreter>,
        >,
    ERROR: EvmTrError<EVM> + From<BaseTransactionError> + FromStringError + IsTxError,
{
    type IT = EthInterpreter;
}

#[cfg(test)]
mod tests {

    use alloy_evm::Evm;
    use alloy_primitives::{address, keccak256, uint};
    use base_common_consensus::Predeploys;
    use revm::{
        InspectEvm,
        bytecode::Bytecode,
        context::{BlockEnv, CfgEnv, Context, TxEnv},
        database::InMemoryDB,
        database_interface::EmptyDB,
        handler::{EthFrame, Handler},
        inspector::NoOpInspector,
        interpreter::{CallOutcome, Gas, InstructionResult, InterpreterResult},
        primitives::{Address, B256, Bytes, TxKind, bytes, hardfork::SpecId},
        state::AccountInfo,
    };
    use rstest::rstest;

    use super::*;
    use crate::{BaseContext, BaseSpecId, BaseTransaction, Builder, DefaultBase, L1BlockInfo};

    /// Creates frame result.
    fn call_last_frame_return(
        ctx: BaseContext<EmptyDB>,
        instruction_result: InstructionResult,
        gas: Gas,
    ) -> Gas {
        let mut evm = ctx.build_base();

        let mut exec_result = FrameResult::Call(CallOutcome::new(
            InterpreterResult { result: instruction_result, output: Bytes::new(), gas },
            0..0,
        ));

        let mut handler =
            BaseHandler::<_, EVMError<_, BaseTransactionError>, EthFrame<EthInterpreter>>::new();

        let tx_gas_limit = evm.ctx().tx().gas_limit();
        let mut parent_gas = GasTracker::new(tx_gas_limit, tx_gas_limit, 0);

        handler.last_frame_result(&mut evm, &mut exec_result, &mut parent_gas).unwrap();
        handler.refund(&mut evm, &mut exec_result, 0).unwrap();
        *exec_result.gas()
    }

    #[test]
    fn test_revert_gas() {
        let ctx = Context::base()
            .with_tx(BaseTransaction::builder().base(TxEnv::builder().gas_limit(100)).build_fill())
            .with_cfg(CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Regolith)));

        let gas = call_last_frame_return(ctx, InstructionResult::Revert, Gas::new(90));
        assert_eq!(gas.remaining(), 90);
        assert_eq!(gas.total_gas_spent(), 10);
        assert_eq!(gas.refunded(), 0);
    }

    #[test]
    fn test_consume_gas() {
        let ctx = Context::base()
            .with_tx(BaseTransaction::builder().base(TxEnv::builder().gas_limit(100)).build_fill())
            .with_cfg(CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Regolith)));

        let gas = call_last_frame_return(ctx, InstructionResult::Stop, Gas::new(90));
        assert_eq!(gas.remaining(), 90);
        assert_eq!(gas.total_gas_spent(), 10);
        assert_eq!(gas.refunded(), 0);
    }

    #[test]
    fn test_consume_gas_deposit_tx() {
        let ctx = Context::base()
            .with_tx(
                BaseTransaction::builder()
                    .base(TxEnv::builder().gas_limit(100))
                    .source_hash(B256::from([1u8; 32]))
                    .build_fill(),
            )
            .with_cfg(CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Regolith)));
        let gas = call_last_frame_return(ctx, InstructionResult::Stop, Gas::new(90));
        assert_eq!(gas.remaining(), 90);
        assert_eq!(gas.total_gas_spent(), 10);
        assert_eq!(gas.refunded(), 0);
    }

    #[test]
    fn test_consume_gas_with_refund() {
        let ctx = Context::base()
            .with_tx(
                BaseTransaction::builder()
                    .base(TxEnv::builder().gas_limit(100))
                    .source_hash(B256::from([1u8; 32]))
                    .build_fill(),
            )
            .with_cfg(CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Regolith)));

        let mut ret_gas = Gas::new(90);
        ret_gas.record_refund(20);

        let gas = call_last_frame_return(ctx.clone(), InstructionResult::Stop, ret_gas);
        assert_eq!(gas.remaining(), 90);
        assert_eq!(gas.total_gas_spent(), 10);
        assert_eq!(gas.refunded(), 2); // min(20, 10/5)

        let gas = call_last_frame_return(ctx, InstructionResult::Revert, ret_gas);
        assert_eq!(gas.remaining(), 90);
        assert_eq!(gas.total_gas_spent(), 10);
        assert_eq!(gas.refunded(), 0);
    }

    #[test]
    fn test_halt_gas_non_deposit() {
        let ctx = Context::base()
            .with_tx(BaseTransaction::builder().base(TxEnv::builder().gas_limit(100)).build_fill())
            .with_cfg(CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Regolith)));

        let gas = call_last_frame_return(ctx, InstructionResult::OutOfGas, Gas::new(90));
        assert_eq!(gas.remaining(), 0);
        assert_eq!(gas.total_gas_spent(), 100);
        assert_eq!(gas.refunded(), 0);
    }

    #[test]
    fn test_consume_gas_deposit_ok_regolith_matches_non_deposit() {
        let ctx = Context::base()
            .with_tx(
                BaseTransaction::builder()
                    .base(TxEnv::builder().gas_limit(100))
                    .source_hash(B256::from([1u8; 32]))
                    .build_fill(),
            )
            .with_cfg(CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Regolith)));

        let gas = call_last_frame_return(ctx, InstructionResult::Stop, Gas::new(90));
        assert_eq!(gas.remaining(), 90);
        assert_eq!(gas.total_gas_spent(), 10);
        assert_eq!(gas.refunded(), 0);
    }

    /// Guards the assumption behind the `eip8037_child_gas` tests below: Base never maps to an
    /// Ethereum spec at or after Amsterdam, so revm's EIP-8037 state gas stays off on every Base
    /// upgrade. Those tests mirror revm's reservoir accounting, not Base-specific behavior. When
    /// this test fails, Glamsterdam is being wired up: re-verify them against revm. Zenith is
    /// included because the gas limit pricing hook relies on `Gas::spend_all` leaving no
    /// reservoir behind.
    #[test]
    fn test_eip8037_state_gas_disabled_on_all_base_upgrades() {
        for upgrade in BaseUpgrade::EXECUTION_VARIANTS.into_iter().chain([BaseUpgrade::Zenith]) {
            let cfg = CfgEnv::new().with_spec_and_mainnet_gas_params(BaseSpecId::new(upgrade));
            assert!(!cfg.enable_amsterdam_eip8037, "EIP-8037 enabled on {upgrade:?}");
        }
    }

    /// Child gas with state gas set by hand. EIP-8037 is off on Base today, so this state is not
    /// reachable in production; see `test_eip8037_state_gas_disabled_on_all_base_upgrades`.
    fn eip8037_child_gas() -> Gas {
        let mut gas = Gas::new(100);
        let tracker = gas.tracker_mut();
        tracker.set_remaining(50);
        tracker.set_reservoir(30);
        tracker.set_state_gas_spent(20);
        tracker.set_state_gas_spilled(10);
        gas
    }

    #[test]
    fn test_reservoir_spill_recovered_on_revert() {
        let ctx = Context::base()
            .with_tx(BaseTransaction::builder().base(TxEnv::builder().gas_limit(100)).build_fill())
            .with_cfg(CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Regolith)));

        // rollback_state_gas: reservoir = 30 + 20 - 10 = 40, spill (10) credited
        // back to remaining (50 -> 60), then erased onto the parent.
        let gas = call_last_frame_return(ctx, InstructionResult::Revert, eip8037_child_gas());
        assert_eq!(gas.reservoir(), 40);
        assert_eq!(gas.remaining(), 60);
        assert_eq!(gas.total_gas_spent(), 40);
    }

    #[test]
    fn test_reservoir_recovered_but_spill_burned_on_halt() {
        let ctx = Context::base()
            .with_tx(BaseTransaction::builder().base(TxEnv::builder().gas_limit(100)).build_fill())
            .with_cfg(CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Regolith)));

        // Reservoir is still recovered (40) for the parent, but the halt's
        // spend_all burns the spill credit, so remaining collapses to 0.
        let gas = call_last_frame_return(ctx, InstructionResult::OutOfGas, eip8037_child_gas());
        assert_eq!(gas.reservoir(), 40);
        assert_eq!(gas.remaining(), 0);
    }

    #[test]
    fn test_returned_reservoir_reconciles_state_gas_spill_on_ok() {
        let ctx = Context::base()
            .with_tx(BaseTransaction::builder().base(TxEnv::builder().gas_limit(100)).build_fill())
            .with_cfg(CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Regolith)));

        let gas = call_last_frame_return(ctx, InstructionResult::Stop, eip8037_child_gas());
        assert_eq!(gas.state_gas_spent(), 20);
        // Returned reservoir first repays spilled regular gas; only the excess remains reserved.
        assert_eq!(gas.state_gas_spilled(), 0);
        assert_eq!(gas.reservoir(), 20);
        assert_eq!(gas.remaining(), 60);
    }

    #[test]
    fn test_commit_mint_value() {
        let caller = Address::ZERO;
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            caller,
            AccountInfo { balance: U256::from(1000), ..Default::default() },
        );

        let mut ctx = Context::base()
            .with_db(db)
            .with_chain(L1BlockInfo {
                l1_base_fee: U256::from(1_000),
                l1_fee_overhead: Some(U256::from(1_000)),
                l1_base_fee_scalar: U256::from(1_000),
                ..Default::default()
            })
            .with_cfg(CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Regolith)));
        ctx.modify_tx(|tx| {
            tx.deposit.source_hash = B256::from([1u8; 32]);
            tx.deposit.mint = Some(10);
        });

        let mut evm = ctx.build_base();

        let handler =
            BaseHandler::<_, EVMError<_, BaseTransactionError>, EthFrame<EthInterpreter>>::new();
        let mut init_and_floor_gas = InitialAndFloorGas::new(0, 0);
        handler
            .validate_against_state_and_deduct_caller(&mut evm, &mut init_and_floor_gas)
            .unwrap();

        // Check the account balance is updated.
        let account = evm.ctx_mut().journal_mut().load_account(caller).unwrap();
        assert_eq!(account.info.balance, U256::from(1010));
    }

    #[test]
    fn test_remove_l1_cost_non_deposit() {
        let caller = Address::ZERO;
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            caller,
            AccountInfo {
                balance: U256::from(1058), // Increased to cover L1 fees (1048) + base fees
                ..Default::default()
            },
        );
        let ctx = Context::base()
            .with_db(db)
            .with_chain(L1BlockInfo {
                l1_base_fee: U256::from(1_000),
                l1_fee_overhead: Some(U256::from(1_000)),
                l1_base_fee_scalar: U256::from(1_000),
                l2_block: Some(U256::from(0)),
                ..Default::default()
            })
            .with_cfg(CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Regolith)))
            .with_tx(
                BaseTransaction::builder()
                    .base(TxEnv::builder().gas_limit(100))
                    .enveloped_tx(Some(bytes!("FACADE")))
                    .source_hash(B256::ZERO)
                    .build()
                    .unwrap(),
            );

        let mut evm = ctx.build_base();

        let handler =
            BaseHandler::<_, EVMError<_, BaseTransactionError>, EthFrame<EthInterpreter>>::new();
        let mut init_and_floor_gas = InitialAndFloorGas::new(0, 0);
        handler
            .validate_against_state_and_deduct_caller(&mut evm, &mut init_and_floor_gas)
            .unwrap();

        // Check the account balance is updated.
        let account = evm.ctx_mut().journal_mut().load_account(caller).unwrap();
        assert_eq!(account.info.balance, U256::from(10)); // 1058 - 1048 = 10
    }

    #[test]
    fn test_reload_l1_block_info_isthmus() {
        const BLOCK_NUM: U256 = uint!(100_U256);
        const L1_BASE_FEE: U256 = uint!(1_U256);
        const L1_BLOB_BASE_FEE: U256 = uint!(2_U256);
        const L1_BASE_FEE_SCALAR: u64 = 3;
        const L1_BLOB_BASE_FEE_SCALAR: u64 = 4;
        const L1_FEE_SCALARS: U256 = U256::from_limbs([
            0,
            (L1_BASE_FEE_SCALAR << (64 - L1BlockInfo::BASE_FEE_SCALAR_OFFSET * 2))
                | L1_BLOB_BASE_FEE_SCALAR,
            0,
            0,
        ]);
        const OPERATOR_FEE_SCALAR: u64 = 5;
        const OPERATOR_FEE_CONST: u64 = 6;
        const OPERATOR_FEE: U256 =
            U256::from_limbs([OPERATOR_FEE_CONST, OPERATOR_FEE_SCALAR, 0, 0]);

        let mut db = InMemoryDB::default();
        let l1_block_contract = db.load_account(Predeploys::L1_BLOCK_INFO).unwrap();
        l1_block_contract.storage.insert(L1BlockInfo::L1_BASE_FEE_SLOT, L1_BASE_FEE);
        l1_block_contract
            .storage
            .insert(L1BlockInfo::ECOTONE_L1_BLOB_BASE_FEE_SLOT, L1_BLOB_BASE_FEE);
        l1_block_contract.storage.insert(L1BlockInfo::ECOTONE_L1_FEE_SCALARS_SLOT, L1_FEE_SCALARS);
        l1_block_contract.storage.insert(L1BlockInfo::OPERATOR_FEE_SCALARS_SLOT, OPERATOR_FEE);
        db.insert_account_info(
            Address::ZERO,
            AccountInfo { balance: U256::from(1000), ..Default::default() },
        );

        let ctx = Context::base()
            .with_db(db)
            .with_chain(L1BlockInfo {
                l2_block: Some(BLOCK_NUM + U256::from(1)), // ahead by one block
                ..Default::default()
            })
            .with_block(BlockEnv { number: BLOCK_NUM, ..Default::default() })
            .with_cfg(CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Isthmus)));

        let mut evm = ctx.build_base();

        assert_ne!(evm.ctx().chain().l2_block, Some(BLOCK_NUM));

        let handler =
            BaseHandler::<_, EVMError<_, BaseTransactionError>, EthFrame<EthInterpreter>>::new();
        let mut init_and_floor_gas = InitialAndFloorGas::new(0, 0);
        handler
            .validate_against_state_and_deduct_caller(&mut evm, &mut init_and_floor_gas)
            .unwrap();

        assert_eq!(
            *evm.ctx().chain(),
            L1BlockInfo {
                l2_block: Some(BLOCK_NUM),
                l1_base_fee: L1_BASE_FEE,
                l1_base_fee_scalar: U256::from(L1_BASE_FEE_SCALAR),
                l1_blob_base_fee: Some(L1_BLOB_BASE_FEE),
                l1_blob_base_fee_scalar: Some(U256::from(L1_BLOB_BASE_FEE_SCALAR)),
                empty_ecotone_scalars: false,
                l1_fee_overhead: None,
                operator_fee_scalar: Some(U256::from(OPERATOR_FEE_SCALAR)),
                operator_fee_constant: Some(U256::from(OPERATOR_FEE_CONST)),
                tx_l1_cost: Some(U256::ZERO),
                da_footprint_gas_scalar: None
            }
        );
    }

    #[rstest]
    #[case::azul_above_cap_rejected(BaseUpgrade::Azul, 16_777_217, false, false)]
    #[case::azul_at_cap_accepted(BaseUpgrade::Azul, 16_777_216, false, true)]
    #[case::jovian_above_cap_accepted(BaseUpgrade::Jovian, 16_777_217, false, true)]
    #[case::azul_deposit_above_cap_accepted(BaseUpgrade::Azul, 16_777_217, true, true)]
    fn test_validate_env_tx_gas_limit_cap(
        #[case] upgrade: BaseUpgrade,
        #[case] gas_limit: u64,
        #[case] is_deposit: bool,
        #[case] accepted: bool,
    ) {
        let builder = BaseTransaction::builder().base(TxEnv::builder().gas_limit(gas_limit));
        let tx = if is_deposit {
            builder.source_hash(B256::from([1u8; 32]))
        } else {
            builder.enveloped_tx(Some(bytes!("FACADE")))
        };
        let ctx = Context::base()
            .with_tx(tx.build_fill())
            .with_cfg(CfgEnv::new_with_spec(BaseSpecId::new(upgrade)));
        let mut evm = ctx.build_base();
        let handler =
            BaseHandler::<_, EVMError<_, BaseTransactionError>, EthFrame<EthInterpreter>>::new();

        assert_eq!(handler.validate_env(&mut evm).is_ok(), accepted);
    }

    #[test]
    fn test_osaka_opcodes_activated_azul() {
        assert_eq!(BaseSpecId::new(BaseUpgrade::Azul).into_eth_spec(), SpecId::OSAKA);
    }

    /// EIP-6780 preserves existing contracts from Ecotone onward, while contracts created
    /// and destroyed in the same transaction are still deleted on both sides of the fork.
    #[rstest]
    #[case::canyon(BaseUpgrade::Canyon, true)]
    #[case::ecotone(BaseUpgrade::Ecotone, false)]
    fn test_selfdestruct_fork_behavior(
        #[case] upgrade: BaseUpgrade,
        #[case] deletes_existing: bool,
        #[values(false, true)] created: bool,
    ) {
        let caller = Address::repeat_byte(0x11);
        let contract = if created { caller.create(0) } else { Address::repeat_byte(0x42) };
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            caller,
            AccountInfo { balance: U256::from(1_000_000), ..Default::default() },
        );
        if !created {
            // Read a parent slot, then transfer the balance to CALLER with SELFDESTRUCT.
            db.insert_account_info(
                contract,
                AccountInfo {
                    balance: U256::from(100),
                    nonce: 1,
                    code: Some(Bytecode::new_legacy(bytes!("6001545033FF"))),
                    ..Default::default()
                },
            );
            db.insert_account_storage(contract, U256::from(1), U256::from(11)).unwrap();
        }
        let tx = BaseTransaction::builder()
            .base(
                TxEnv::builder()
                    .caller(caller)
                    .gas_limit(100_000)
                    .kind(if created { TxKind::Create } else { TxKind::Call(contract) })
                    .data(if created { bytes!("33FF") } else { Bytes::new() }),
            )
            .enveloped_tx(Some(bytes!("FACADE")))
            .build_fill();
        let mut evm = Context::base()
            .with_db(db)
            .with_cfg(CfgEnv::new_with_spec(BaseSpecId::new(upgrade)))
            .with_chain(L1BlockInfo { l2_block: Some(U256::ZERO), ..Default::default() })
            .build_with_inspector(NoOpInspector {});
        let result = evm.inspect_tx(tx).unwrap();
        assert!(result.result.is_success());
        let account = &result.state[&contract];
        assert_eq!(account.is_selfdestructed(), created || deletes_existing);
        if !created && !deletes_existing {
            assert_eq!(account.info.nonce, 1);
            assert_eq!(account.info.balance, U256::ZERO);
            assert_eq!(account.storage[&U256::from(1)].present_value(), U256::from(11));
        }
    }

    /// Runs CLZ bytecode (`PUSH1 0x80, CLZ, PUSH1 0x00, MSTORE, PUSH1 0x20, PUSH1 0x00, RETURN`)
    /// against the given spec and returns the execution result.
    fn run_clz_bytecode(
        spec: BaseSpecId,
    ) -> revm::context_interface::result::ExecutionResult<BaseHaltReason> {
        let contract = Address::from([0x42; 20]);
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            contract,
            AccountInfo {
                code: Some(Bytecode::new_legacy(bytes!("60801E60005260206000F3"))),
                ..Default::default()
            },
        );
        db.insert_account_info(
            Address::ZERO,
            AccountInfo { balance: U256::from(1_000_000), ..Default::default() },
        );

        let ctx = Context::base()
            .with_db(db)
            .with_tx(
                BaseTransaction::builder()
                    .base(TxEnv::builder().gas_limit(100_000).kind(TxKind::Call(contract)))
                    .enveloped_tx(Some(bytes!("FACADE")))
                    .build_fill(),
            )
            .with_cfg(CfgEnv::new_with_spec(spec))
            .with_chain(L1BlockInfo {
                l2_block: Some(U256::ZERO),
                operator_fee_scalar: Some(U256::ZERO),
                operator_fee_constant: Some(U256::ZERO),
                ..Default::default()
            });
        let mut evm = ctx.build_base();

        let mut handler =
            BaseHandler::<_, EVMError<_, BaseTransactionError>, EthFrame<EthInterpreter>>::new();
        handler.run(&mut evm).unwrap()
    }

    #[test]
    fn test_clz_opcode_azul() {
        let result = run_clz_bytecode(BaseSpecId::new(BaseUpgrade::Azul));
        assert!(result.is_success(), "CLZ opcode should execute successfully on AZUL");

        let output = result.output().unwrap();
        let expected = U256::from(248);
        let actual = U256::from_be_slice(output);
        assert_eq!(actual, expected, "CLZ of 0x80 in 256-bit should be 248");
    }

    #[test]
    fn test_clz_opcode_not_on_jovian() {
        let result = run_clz_bytecode(BaseSpecId::new(BaseUpgrade::Jovian));
        assert!(!result.is_success(), "CLZ opcode should not be available on JOVIAN (pre-OSAKA)");
    }

    fn tx_error_cleanup_db_with_warmed_account(
        warmed: Address,
        warmed_account_info: AccountInfo,
        probe: Address,
        caller: Address,
    ) -> InMemoryDB {
        let mut probe_code = vec![0x73];
        probe_code.extend_from_slice(warmed.as_slice());
        probe_code.extend_from_slice(&[0x3b, 0x50, 0x00]);

        let mut db = InMemoryDB::default();
        db.insert_account_info(warmed, warmed_account_info);
        db.insert_account_info(
            probe,
            AccountInfo {
                code: Some(Bytecode::new_legacy(probe_code.into())),
                ..Default::default()
            },
        );
        db.insert_account_info(
            caller,
            AccountInfo { balance: U256::from(1_000_000), ..Default::default() },
        );
        db
    }

    fn tx_error_cleanup_db(warmed: Address, probe: Address, caller: Address) -> InMemoryDB {
        tx_error_cleanup_db_with_warmed_account(
            warmed,
            AccountInfo {
                balance: U256::from(1_000_000),
                code: Some(Bytecode::new_legacy(bytes!("00"))),
                ..Default::default()
            },
            probe,
            caller,
        )
    }

    fn tx_error_cleanup_context(db: InMemoryDB) -> BaseContext<InMemoryDB> {
        Context::base()
            .with_db(db)
            .with_chain(L1BlockInfo {
                l2_block: Some(U256::ZERO),
                operator_fee_scalar: Some(U256::ZERO),
                operator_fee_constant: Some(U256::ZERO),
                ..Default::default()
            })
            .with_cfg(CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Isthmus)))
    }

    fn invalid_contract_caller_tx(caller: Address) -> BaseTransaction<TxEnv> {
        BaseTransaction::builder()
            .base(TxEnv::builder().caller(caller).gas_limit(100_000))
            .enveloped_tx(Some(bytes!("FACADE")))
            .build_fill()
    }

    fn stale_nonce_tx(caller: Address) -> BaseTransaction<TxEnv> {
        BaseTransaction::builder()
            .base(TxEnv::builder().caller(caller).nonce(0).gas_limit(100_000))
            .enveloped_tx(Some(bytes!("FACADE")))
            .build_fill()
    }

    fn probe_tx(caller: Address, probe: Address) -> BaseTransaction<TxEnv> {
        BaseTransaction::builder()
            .base(TxEnv::builder().caller(caller).kind(TxKind::Call(probe)).gas_limit(100_000))
            .enveloped_tx(Some(bytes!("FACADE")))
            .build_fill()
    }

    #[test]
    fn non_deposit_tx_error_discards_journal_in_inspector_path() {
        let warmed = Address::repeat_byte(0x33);
        let probe = Address::repeat_byte(0x44);
        let caller = Address::repeat_byte(0x55);

        let mut dirty_evm = tx_error_cleanup_context(tx_error_cleanup_db(warmed, probe, caller))
            .build_with_inspector(NoOpInspector {});
        InspectEvm::inspect_tx(&mut dirty_evm, invalid_contract_caller_tx(warmed))
            .expect_err("contract caller should fail EIP-3607 validation");
        let dirty_result = InspectEvm::inspect_tx(&mut dirty_evm, probe_tx(caller, probe))
            .expect("probe transaction should execute");

        let mut clean_evm = tx_error_cleanup_context(tx_error_cleanup_db(warmed, probe, caller))
            .build_with_inspector(NoOpInspector {});
        let clean_result = InspectEvm::inspect_tx(&mut clean_evm, probe_tx(caller, probe))
            .expect("probe transaction should execute");

        assert_eq!(
            dirty_result.result.tx_gas_used(),
            clean_result.result.tx_gas_used(),
            "failed transaction must not leave the probed account warm"
        );
    }

    #[test]
    fn nonce_too_low_error_discards_authorizer_warmth_in_inspector_path() {
        let authorizer = Address::repeat_byte(0xd6);
        let probe = Address::repeat_byte(0x44);
        let relayer = Address::repeat_byte(0x63);

        let db = tx_error_cleanup_db_with_warmed_account(
            authorizer,
            AccountInfo { nonce: 1, balance: U256::from(1_000_000), ..Default::default() },
            probe,
            relayer,
        );
        let mut dirty_evm = tx_error_cleanup_context(db).build_with_inspector(NoOpInspector {});
        InspectEvm::inspect_tx(&mut dirty_evm, stale_nonce_tx(authorizer))
            .expect_err("stale authorizer transaction should fail nonce validation");
        let dirty_result = InspectEvm::inspect_tx(&mut dirty_evm, probe_tx(relayer, probe))
            .expect("authorizer code-size probe transaction should execute");

        let db = tx_error_cleanup_db_with_warmed_account(
            authorizer,
            AccountInfo { nonce: 1, balance: U256::from(1_000_000), ..Default::default() },
            probe,
            relayer,
        );
        let mut clean_evm = tx_error_cleanup_context(db).build_with_inspector(NoOpInspector {});
        let clean_result = InspectEvm::inspect_tx(&mut clean_evm, probe_tx(relayer, probe))
            .expect("authorizer code-size probe transaction should execute");

        assert_eq!(
            dirty_result.result.tx_gas_used(),
            clean_result.result.tx_gas_used(),
            "stale authorizer transaction must not make the next EXTCODESIZE(authorizer) warm"
        );
    }

    // ---- Gas limit pricing (docs/guides/GAS_LIMIT_PRICING.md) ----

    const GLP_CALLER: Address = address!("0x00000000000000000000000000000000000000c1");
    const GLP_TARGET: Address = address!("0x00000000000000000000000000000000000000c2");
    const GLP_BENEFICIARY: Address = address!("0x00000000000000000000000000000000000000bb");
    const GLP_BASEFEE: u64 = 1_000;
    const GLP_TIP: u128 = 7;
    const GLP_GAS_LIMIT: u64 = 100_000;
    const GLP_INITIAL_BALANCE: U256 = uint!(1_000_000_000_000_U256);

    /// Fee settlement observed after executing one transaction: what the caller paid and
    /// what each recipient received, plus the gas the receipt would report.
    #[derive(Debug, PartialEq, Eq)]
    struct Settlement {
        tx_gas_used: u64,
        final_refunded: u64,
        caller_paid: U256,
        beneficiary: U256,
        base_fee_vault: U256,
        success: bool,
    }

    /// Executes a plain non-deposit transaction from `GLP_CALLER` to `GLP_TARGET` (deployed
    /// with `code`, or empty) at `spec`, with zero L1 and operator fees so the settlement is
    /// the EIP-1559 component alone.
    fn execute_glp_tx(
        spec: BaseUpgrade,
        code: Option<Bytes>,
        storage: Option<(U256, U256)>,
        gas_limit: u64,
        deposit: bool,
        disable_fee_charge: bool,
    ) -> Settlement {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            GLP_CALLER,
            AccountInfo { balance: GLP_INITIAL_BALANCE, ..Default::default() },
        );
        if let Some(code) = code {
            db.insert_account_info(
                GLP_TARGET,
                AccountInfo {
                    code_hash: keccak256(&code),
                    code: Some(Bytecode::new_raw(code)),
                    ..Default::default()
                },
            );
        }
        if let Some((slot, value)) = storage {
            db.insert_account_storage(GLP_TARGET, slot, value).unwrap();
        }

        let mut cfg = CfgEnv::new_with_spec(BaseSpecId::new(spec));
        cfg.disable_fee_charge = disable_fee_charge;
        let base =
            TxEnv::builder().caller(GLP_CALLER).kind(TxKind::Call(GLP_TARGET)).gas_limit(gas_limit);
        // Deposits carry no gas price (`TxEnv::from_recovered_tx(TxDeposit)` leaves it zero).
        let tx = if deposit {
            BaseTransaction::builder().base(base).source_hash(B256::from([1u8; 32])).build_fill()
        } else {
            BaseTransaction::builder()
                .base(
                    base.gas_price(u128::from(GLP_BASEFEE) + GLP_TIP)
                        .gas_priority_fee(Some(GLP_TIP)),
                )
                .enveloped_tx(Some(bytes!("FACADE")))
                .build_fill()
        };
        let mut evm = Context::base()
            .with_db(db)
            .with_chain(L1BlockInfo { l2_block: Some(U256::ZERO), ..Default::default() })
            .with_block(BlockEnv {
                basefee: GLP_BASEFEE,
                beneficiary: GLP_BENEFICIARY,
                ..Default::default()
            })
            .with_cfg(cfg)
            .build_with_inspector(NoOpInspector);

        let outcome = evm.transact_raw(tx).expect("transaction should execute");
        let balance =
            |addr: Address| outcome.state.get(&addr).map_or(U256::ZERO, |acc| acc.info.balance);
        Settlement {
            tx_gas_used: outcome.result.tx_gas_used(),
            final_refunded: outcome.result.gas().final_refunded(),
            caller_paid: GLP_INITIAL_BALANCE - balance(GLP_CALLER),
            beneficiary: balance(GLP_BENEFICIARY),
            base_fee_vault: balance(Predeploys::BASE_FEE_VAULT),
            success: outcome.result.is_success(),
        }
    }

    fn glp_settlement_for(gas: u64) -> Settlement {
        Settlement {
            tx_gas_used: gas,
            final_refunded: 0,
            caller_paid: U256::from(gas) * U256::from(u128::from(GLP_BASEFEE) + GLP_TIP),
            beneficiary: U256::from(gas) * U256::from(GLP_TIP),
            base_fee_vault: U256::from(gas) * U256::from(GLP_BASEFEE),
            success: true,
        }
    }

    /// Latest scheduled upgrade: gas limit pricing is off, so the plain transfer is billed
    /// and reported at its metered 21,000 gas. Pins the pre-activation path.
    #[test]
    fn test_gas_limit_pricing_off_bills_metered_gas() {
        assert!(!BaseSpecId::new(BaseUpgrade::Everest).charges_gas_limit());
        let settlement =
            execute_glp_tx(BaseUpgrade::Everest, None, None, GLP_GAS_LIMIT, false, false);
        assert_eq!(settlement, glp_settlement_for(21_000));
    }

    /// Zenith: the caller pays for the declared limit, the sequencer and the base-fee vault
    /// are paid on the limit, and the receipt reports the limit (S1, S2, S4).
    #[test]
    fn test_gas_limit_pricing_bills_declared_gas_limit() {
        assert!(BaseSpecId::new(BaseUpgrade::Zenith).charges_gas_limit());
        let settlement =
            execute_glp_tx(BaseUpgrade::Zenith, None, None, GLP_GAS_LIMIT, false, false);
        assert_eq!(settlement, glp_settlement_for(GLP_GAS_LIMIT));
    }

    /// An EIP-3529 SSTORE-clear refund lowers the metered bill before Zenith and has no effect
    /// on the bill or the reported gas under gas limit pricing (S5).
    #[test]
    fn test_gas_limit_pricing_ignores_refunds() {
        // PUSH1 0, PUSH1 0, SSTORE, STOP: clears slot 0 (pre-seeded to 1) for a refund.
        let code = bytes!("600060005500");
        let seeded = Some((U256::ZERO, U256::from(1)));

        let metered = execute_glp_tx(
            BaseUpgrade::Everest,
            Some(code.clone()),
            seeded,
            GLP_GAS_LIMIT,
            false,
            false,
        );
        assert!(metered.success);
        assert!(metered.final_refunded > 0, "the clear must earn a refund pre-Zenith");
        assert!(metered.tx_gas_used < GLP_GAS_LIMIT);

        let priced =
            execute_glp_tx(BaseUpgrade::Zenith, Some(code), seeded, GLP_GAS_LIMIT, false, false);
        assert_eq!(priced, glp_settlement_for(GLP_GAS_LIMIT));
    }

    /// Out of gas already bills the full limit today, so the worst case is unchanged by gas
    /// limit pricing; only the success-path discount goes away (S11).
    #[test]
    fn test_gas_limit_pricing_out_of_gas_matches_pre_activation() {
        // JUMPDEST, PUSH1 0, JUMP: spins until out of gas.
        let code = bytes!("5b600056");
        let metered = execute_glp_tx(
            BaseUpgrade::Everest,
            Some(code.clone()),
            None,
            GLP_GAS_LIMIT,
            false,
            false,
        );
        let priced =
            execute_glp_tx(BaseUpgrade::Zenith, Some(code), None, GLP_GAS_LIMIT, false, false);
        assert!(!metered.success && !priced.success);
        assert_eq!(metered, priced);
        assert_eq!(priced.caller_paid, glp_settlement_for(GLP_GAS_LIMIT).caller_paid);
    }

    /// The probe shape from the design doc: a high-tip transaction that checks for an
    /// opportunity and reverts when it is not there. Today it pays metered gas, so the revert is
    /// nearly free; under gas limit pricing it pays the full declared limit, the same as a
    /// successful execution would (S1, S11, S13).
    #[test]
    fn test_gas_limit_pricing_reverting_probe_pays_declared_limit() {
        // PUSH1 0, PUSH1 0, REVERT: the opportunity check failed.
        let code = bytes!("60006000fd");
        let metered = execute_glp_tx(
            BaseUpgrade::Everest,
            Some(code.clone()),
            None,
            GLP_GAS_LIMIT,
            false,
            false,
        );
        assert!(!metered.success);
        assert!(metered.tx_gas_used < GLP_GAS_LIMIT / 4, "the revert must be cheap when metered");
        assert_eq!(
            metered,
            Settlement { success: false, ..glp_settlement_for(metered.tx_gas_used) }
        );

        let priced =
            execute_glp_tx(BaseUpgrade::Zenith, Some(code), None, GLP_GAS_LIMIT, false, false);
        assert_eq!(priced, Settlement { success: false, ..glp_settlement_for(GLP_GAS_LIMIT) });
    }

    /// Simulation runs with fee charging disabled and must keep metered gas so
    /// `eth_estimateGas` still finds what a call needs (S6).
    #[test]
    fn test_gas_limit_pricing_simulation_keeps_metered_gas() {
        let simulated = execute_glp_tx(BaseUpgrade::Zenith, None, None, GLP_GAS_LIMIT, false, true);
        assert_eq!(simulated.tx_gas_used, 21_000);
        assert_eq!(simulated.caller_paid, U256::ZERO);
        assert_eq!(simulated.beneficiary, U256::ZERO);
    }

    /// Deposits are not ordered by tip and pay no L2 execution fee, so gas limit pricing leaves
    /// them alone: a deposit reports its metered gas and pays nothing on either upgrade (S9).
    #[test]
    fn test_gas_limit_pricing_leaves_deposits_metered_and_unpaid() {
        let metered = execute_glp_tx(BaseUpgrade::Everest, None, None, GLP_GAS_LIMIT, true, false);
        assert_eq!(metered.tx_gas_used, 21_000);

        let priced = execute_glp_tx(BaseUpgrade::Zenith, None, None, GLP_GAS_LIMIT, true, false);
        assert_eq!(priced.tx_gas_used, 21_000, "deposits must keep metered gas");
        assert!(priced.success);
        assert_eq!(priced.caller_paid, U256::ZERO);
        assert_eq!(priced.beneficiary, U256::ZERO);
        assert_eq!(priced.base_fee_vault, U256::ZERO);
    }

    /// Gas limit pricing is off on every upgrade in the execution fork ladder.
    #[test]
    fn test_gas_limit_pricing_off_on_all_scheduled_upgrades() {
        for upgrade in BaseUpgrade::EXECUTION_VARIANTS {
            assert!(!BaseSpecId::new(upgrade).charges_gas_limit(), "enabled on {upgrade:?}");
        }
    }
}
