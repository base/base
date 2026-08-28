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
#[cfg(feature = "std")]
use revm::{
    context_interface::transaction::{AuthorizationTr, TransactionType},
    database_interface::Database,
    primitives::Address,
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

/// Loads `account_state[account]` from the EIP-8130 AccountConfiguration
/// predeploy. [`JournalTr::sload`] assumes the account is already present.
#[cfg(feature = "std")]
fn load_standard_account_state<JOURNAL: JournalTr>(
    journal: &mut JOURNAL,
    account: Address,
) -> Result<
    base_execution_eip8130::AccountState,
    <<JOURNAL as JournalTr>::Database as Database>::Error,
> {
    let config = base_execution_eip8130::AccountConfigurationStorage::ADDRESS;
    journal.load_account(config)?;
    let slot = U256::from_be_bytes(
        base_execution_eip8130::AccountConfigurationStorage::account_state_slot(account).0,
    );
    let word = journal.sload(config, slot)?.data;
    Ok(base_execution_eip8130::AccountState::from_word(word))
}

/// EIP-7702 auth-list apply with the standard-keystore gate on each recovered
/// authority. Invalid auths still `continue` (the transaction is included);
/// a revoked / expired / non-admin default EOA is the same skip as a bad
/// nonce or failed `ecrecover`.
#[cfg(feature = "std")]
fn apply_eip7702_auth_list_standard_keystore<CTX, ERROR>(
    context: &mut CTX,
    init_and_floor_gas: &mut InitialAndFloorGas,
) -> Result<u64, ERROR>
where
    CTX: ContextTr,
    ERROR:
        From<InvalidTransaction> + From<<CTX::Db as Database>::Error> + From<BaseTransactionError>,
{
    let chain_id = context.cfg().chain_id();
    let is_eip8037 = context.cfg().is_amsterdam_eip8037_enabled();
    let now: u64 = context
        .block()
        .timestamp()
        .try_into()
        .map_err(|_| BaseTransactionError::standard_sender("block timestamp exceeds u64"))?;
    let (tx, journal) = context.tx_journal_mut();

    if tx.tx_type() != TransactionType::Eip7702 {
        return Ok(0);
    }

    let (number_of_refunded_accounts, number_of_refunded_bytecodes) =
        apply_auth_list_standard_keystore::<_, ERROR>(
            chain_id,
            now,
            tx.authorization_list(),
            journal,
        )?;

    let params = context.cfg().gas_params();
    if is_eip8037 {
        init_and_floor_gas.state_refund += params
            .tx_eip7702_state_refund(number_of_refunded_accounts, number_of_refunded_bytecodes);
    }

    Ok(params.tx_eip7702_auth_refund_regular().saturating_mul(number_of_refunded_accounts))
}

/// Stock [`revm::handler::pre_execution::apply_auth_list`] plus a keystore
/// check after authority recovery. The authority account is warmed first so a
/// skipped auth still matches EIP-7702's "invalid after `ecrecover`" gas.
#[cfg(feature = "std")]
fn apply_auth_list_standard_keystore<JOURNAL, ERROR>(
    chain_id: u64,
    now: u64,
    auth_list: impl Iterator<Item = impl AuthorizationTr>,
    journal: &mut JOURNAL,
) -> Result<(u64, u64), ERROR>
where
    JOURNAL: JournalTr,
    ERROR: From<InvalidTransaction> + From<<JOURNAL::Database as Database>::Error>,
{
    let mut refunded_accounts = 0;
    let mut refunded_bytecodes = 0;
    for authorization in auth_list {
        let auth_chain_id = authorization.chain_id();
        if !auth_chain_id.is_zero() && auth_chain_id != U256::from(chain_id) {
            continue;
        }

        if authorization.nonce() == u64::MAX {
            continue;
        }

        let Some(authority) = authorization.authority() else {
            continue;
        };

        let authority_acc = journal.load_account_with_code_mut(authority)?;
        let authority_acc_info = &authority_acc.account().info;

        if let Some(bytecode) = &authority_acc_info.code {
            if !bytecode.is_empty() && !bytecode.is_eip7702() {
                continue;
            }
        }

        if authorization.nonce() != authority_acc_info.nonce {
            continue;
        }

        // Drop so we can SLOAD AccountConfiguration, then skip like a bad nonce.
        drop(authority_acc);
        let state = load_standard_account_state(journal, authority)?;
        if base_execution_eip8130::ActorAuthorizer::authorize_standard_sender_from_state(
            authority, &state, now,
        )
        .is_err()
        {
            continue;
        }

        let mut authority_acc = journal.load_account_with_code_mut(authority)?;
        let authority_acc_info = &authority_acc.account().info;

        if !(authority_acc_info.is_empty()
            && authority_acc.account().is_loaded_as_not_existing_not_touched())
        {
            refunded_accounts += 1;
        }

        if !authority_acc_info.is_code_hash_empty_or_zero() || authorization.address().is_zero() {
            refunded_bytecodes += 1;
        }

        authority_acc.delegate(authorization.address());
    }

    Ok((refunded_accounts, refunded_bytecodes))
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

        // Cobalt+: standard txs (legacy / 2930 / 1559 / 7702) must still present
        // a live unrestricted default EOA in the EIP-8130 keystore. Recovery
        // stays stateless ecrecover; this is the stateful gate. 8130 txs use
        // `ActorTxVerifier` instead and never reach this handler. `no_std`
        // (proof/zkVM) builds skip the check until the eip8130 crate is
        // available there — same gating as enshrined 8130 execution.
        #[cfg(feature = "std")]
        if spec.is_enabled_in(BaseUpgrade::Cobalt)
            && tx.tx_type() != DEPOSIT_TRANSACTION_TYPE
            && tx.tx_type() != crate::EIP8130_TRANSACTION_TYPE
        {
            let caller = tx.caller();
            let now: u64 = block.timestamp().try_into().map_err(|_| {
                BaseTransactionError::standard_sender("block timestamp exceeds u64")
            })?;
            let state = load_standard_account_state(journal, caller)?;
            base_execution_eip8130::ActorAuthorizer::authorize_standard_sender_from_state(
                caller, &state, now,
            )
            .map_err(BaseTransactionError::standard_sender)?;
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

    fn apply_eip7702_auth_list(
        &self,
        evm: &mut Self::Evm,
        init_and_floor_gas: &mut InitialAndFloorGas,
    ) -> Result<u64, Self::Error> {
        // Cobalt+: each recovered 7702 authority must still present a live
        // unrestricted default EOA. A revoked / expired / scoped k1 is
        // skipped (`continue`), same as a bad signature or nonce — the
        // transaction is still included, that delegation is not applied.
        #[cfg(feature = "std")]
        if evm.ctx().cfg().spec().is_enabled_in(BaseUpgrade::Cobalt) {
            return apply_eip7702_auth_list_standard_keystore(evm.ctx_mut(), init_and_floor_gas);
        }
        self.mainnet.apply_eip7702_auth_list(evm, init_and_floor_gas)
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

    use alloy_eips::eip7702::{Authorization, RecoveredAuthority, RecoveredAuthorization};
    use alloy_primitives::uint;
    use base_common_consensus::{Eip8130Constants, Predeploys};
    use base_execution_eip8130::AccountConfigurationStorage;
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
    fn test_state_gas_propagated_to_parent_on_ok() {
        let ctx = Context::base()
            .with_tx(BaseTransaction::builder().base(TxEnv::builder().gas_limit(100)).build_fill())
            .with_cfg(CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Regolith)));

        let gas = call_last_frame_return(ctx, InstructionResult::Stop, eip8037_child_gas());
        assert_eq!(gas.state_gas_spent(), 20);
        assert_eq!(gas.state_gas_spilled(), 10);
        assert_eq!(gas.reservoir(), 30);
        assert_eq!(gas.remaining(), 50);
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

    #[test]
    fn test_azul_tx_gas_limit_cap_rejected() {
        let ctx = Context::base()
            .with_tx(
                BaseTransaction::builder()
                    .base(TxEnv::builder().gas_limit(16_777_217))
                    .enveloped_tx(Some(bytes!("FACADE")))
                    .build_fill(),
            )
            .with_cfg(CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Azul)));
        let mut evm = ctx.build_base();
        let handler =
            BaseHandler::<_, EVMError<_, BaseTransactionError>, EthFrame<EthInterpreter>>::new();
        let result = handler.validate_env(&mut evm);
        assert!(result.is_err(), "gas_limit above cap should be rejected");
    }

    #[test]
    fn test_azul_tx_gas_limit_at_cap_ok() {
        let ctx = Context::base()
            .with_tx(
                BaseTransaction::builder()
                    .base(TxEnv::builder().gas_limit(16_777_216))
                    .enveloped_tx(Some(bytes!("FACADE")))
                    .build_fill(),
            )
            .with_cfg(CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Azul)));
        let mut evm = ctx.build_base();
        let handler =
            BaseHandler::<_, EVMError<_, BaseTransactionError>, EthFrame<EthInterpreter>>::new();
        let result = handler.validate_env(&mut evm);
        assert!(result.is_ok(), "gas_limit at cap should be accepted");
    }

    #[test]
    fn test_jovian_no_tx_gas_limit_cap() {
        let ctx = Context::base()
            .with_tx(
                BaseTransaction::builder()
                    .base(TxEnv::builder().gas_limit(16_777_217))
                    .enveloped_tx(Some(bytes!("FACADE")))
                    .build_fill(),
            )
            .with_cfg(CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Jovian)));
        let mut evm = ctx.build_base();
        let handler =
            BaseHandler::<_, EVMError<_, BaseTransactionError>, EthFrame<EthInterpreter>>::new();
        let result = handler.validate_env(&mut evm);
        assert!(result.is_ok(), "Jovian should not enforce gas limit cap");
    }

    #[test]
    fn test_azul_deposit_skips_gas_limit_cap() {
        let ctx = Context::base()
            .with_tx(
                BaseTransaction::builder()
                    .base(TxEnv::builder().gas_limit(16_777_217))
                    .source_hash(B256::from([1u8; 32]))
                    .build_fill(),
            )
            .with_cfg(CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Azul)));
        let mut evm = ctx.build_base();
        let handler =
            BaseHandler::<_, EVMError<_, BaseTransactionError>, EthFrame<EthInterpreter>>::new();
        let result = handler.validate_env(&mut evm);
        assert!(result.is_ok(), "deposit txs should skip gas limit cap");
    }

    #[test]
    fn test_osaka_opcodes_activated_azul() {
        assert_eq!(BaseSpecId::new(BaseUpgrade::Azul).into_eth_spec(), SpecId::OSAKA);
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

    /// Packs the inline-self fields of `AccountState` (flags at bit 128, expiry
    /// at 184, scope at 232). Sequences and lock fields stay zero.
    fn pack_inline_self(scope: u16, expiry: u64, revoked: bool) -> U256 {
        let flags = if revoked { Eip8130Constants::DEFAULT_EOA_REVOKED } else { 0 };
        (U256::from(flags) << 128) | (U256::from(expiry) << 184) | (U256::from(scope) << 232)
    }

    fn seed_account_state(db: &mut InMemoryDB, account: Address, word: U256) {
        let slot = U256::from_be_bytes(AccountConfigurationStorage::account_state_slot(account).0);
        db.load_account(AccountConfigurationStorage::ADDRESS).unwrap().storage.insert(slot, word);
    }

    fn standard_keystore_db(caller: Address, word: Option<U256>) -> InMemoryDB {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            caller,
            AccountInfo { balance: U256::from(1_000_000), ..Default::default() },
        );
        if let Some(word) = word {
            seed_account_state(&mut db, caller, word);
        }
        db
    }

    fn standard_keystore_tx(caller: Address) -> BaseTransaction<TxEnv> {
        BaseTransaction::builder()
            .base(TxEnv::builder().caller(caller).gas_limit(100_000))
            .enveloped_tx(Some(bytes!("FACADE")))
            .build_fill()
    }

    fn standard_keystore_context(
        db: InMemoryDB,
        spec: BaseUpgrade,
    ) -> crate::BaseContext<InMemoryDB> {
        Context::base()
            .with_db(db)
            .with_chain(L1BlockInfo {
                l2_block: Some(U256::ZERO),
                operator_fee_scalar: Some(U256::ZERO),
                operator_fee_constant: Some(U256::ZERO),
                ..Default::default()
            })
            .with_cfg(CfgEnv::new_with_spec(BaseSpecId::new(spec)))
    }

    fn authorize_standard_sender(
        db: InMemoryDB,
        spec: BaseUpgrade,
        caller: Address,
    ) -> Result<(), EVMError<core::convert::Infallible, BaseTransactionError>> {
        let ctx = standard_keystore_context(db, spec).with_tx(standard_keystore_tx(caller));
        let mut evm = ctx.build_base();
        let handler =
            BaseHandler::<_, EVMError<_, BaseTransactionError>, EthFrame<EthInterpreter>>::new();
        let mut init_and_floor_gas = InitialAndFloorGas::new(0, 0);
        handler.validate_against_state_and_deduct_caller(&mut evm, &mut init_and_floor_gas)
    }

    #[test]
    fn cobalt_standard_sender_untouched_eoa_is_accepted() {
        let caller = Address::repeat_byte(0x11);
        authorize_standard_sender(standard_keystore_db(caller, None), BaseUpgrade::Cobalt, caller)
            .expect("untouched EOA must still send standard txs");
    }

    #[test]
    fn cobalt_standard_sender_revoked_default_eoa_is_rejected() {
        let caller = Address::repeat_byte(0x11);
        let db = standard_keystore_db(caller, Some(pack_inline_self(0, 0, true)));
        let err = authorize_standard_sender(db, BaseUpgrade::Cobalt, caller)
            .expect_err("revoked default EOA must not send standard txs");
        assert!(
            matches!(err, EVMError::Transaction(BaseTransactionError::StandardSender(_))),
            "expected StandardSender, got {err:?}"
        );
    }

    #[test]
    fn cobalt_standard_sender_scoped_self_is_rejected() {
        let caller = Address::repeat_byte(0x11);
        let db = standard_keystore_db(
            caller,
            Some(pack_inline_self(Eip8130Constants::SCOPE_SENDER, 0, false)),
        );
        let err = authorize_standard_sender(db, BaseUpgrade::Cobalt, caller)
            .expect_err("scoped inline k1 must not send unrestricted standard txs");
        assert!(
            matches!(err, EVMError::Transaction(BaseTransactionError::StandardSender(_))),
            "expected StandardSender, got {err:?}"
        );
    }

    #[test]
    fn pre_cobalt_standard_sender_skips_keystore() {
        let caller = Address::repeat_byte(0x11);
        let db = standard_keystore_db(caller, Some(pack_inline_self(0, 0, true)));
        authorize_standard_sender(db, BaseUpgrade::Isthmus, caller)
            .expect("pre-Cobalt standard txs must not consult the keystore");
    }

    fn recovered_auth(authority: Address, delegate: Address) -> RecoveredAuthorization {
        RecoveredAuthorization::new_unchecked(
            Authorization { chain_id: U256::ZERO, address: delegate, nonce: 0 },
            RecoveredAuthority::Valid(authority),
        )
    }

    fn standard_7702_tx(
        caller: Address,
        auths: Vec<RecoveredAuthorization>,
    ) -> BaseTransaction<TxEnv> {
        BaseTransaction::builder()
            .base(
                TxEnv::builder()
                    .caller(caller)
                    .kind(TxKind::Call(Address::repeat_byte(0x22)))
                    .gas_limit(100_000)
                    .authorization_list_recovered(auths),
            )
            .enveloped_tx(Some(bytes!("FACADE")))
            .build_fill()
    }

    fn authority_has_delegation(db: InMemoryDB, spec: BaseUpgrade, authority: Address) -> bool {
        let caller = Address::repeat_byte(0x33);
        let delegate = Address::repeat_byte(0x44);
        let ctx = standard_keystore_context(db, spec)
            .with_tx(standard_7702_tx(caller, vec![recovered_auth(authority, delegate)]));
        let mut evm = ctx.build_base();
        let handler =
            BaseHandler::<_, EVMError<_, BaseTransactionError>, EthFrame<EthInterpreter>>::new();
        let mut init_and_floor_gas = InitialAndFloorGas::new(0, 0);
        handler.apply_eip7702_auth_list(&mut evm, &mut init_and_floor_gas).unwrap();
        evm.ctx_mut()
            .journal_mut()
            .load_account_with_code(authority)
            .unwrap()
            .info
            .code
            .as_ref()
            .is_some_and(|code| code.is_eip7702())
    }

    #[test]
    fn cobalt_7702_auth_untouched_eoa_is_applied() {
        let authority = Address::repeat_byte(0x11);
        assert!(
            authority_has_delegation(
                standard_keystore_db(authority, None),
                BaseUpgrade::Cobalt,
                authority
            ),
            "untouched EOA must still authorize 7702 delegations"
        );
    }

    #[test]
    fn cobalt_7702_auth_revoked_default_eoa_is_skipped() {
        let authority = Address::repeat_byte(0x11);
        // `authority_has_delegation` applies the auth list and unwraps the
        // result, so it also asserts the transaction is not rejected.
        let db = standard_keystore_db(authority, Some(pack_inline_self(0, 0, true)));
        assert!(
            !authority_has_delegation(db, BaseUpgrade::Cobalt, authority),
            "revoked default EOA must skip its 7702 delegation without failing the transaction"
        );
    }

    #[test]
    fn cobalt_7702_auth_scoped_self_is_skipped() {
        let authority = Address::repeat_byte(0x11);
        let db = standard_keystore_db(
            authority,
            Some(pack_inline_self(Eip8130Constants::SCOPE_SENDER, 0, false)),
        );
        assert!(
            !authority_has_delegation(db, BaseUpgrade::Cobalt, authority),
            "scoped inline k1 must not apply a 7702 delegation"
        );
    }

    #[test]
    fn cobalt_7702_auth_invalid_signature_is_skipped() {
        let live = Address::repeat_byte(0x11);
        let delegate = Address::repeat_byte(0x44);
        let caller = Address::repeat_byte(0x33);
        let ctx = standard_keystore_context(standard_keystore_db(live, None), BaseUpgrade::Cobalt)
            .with_tx(standard_7702_tx(
                caller,
                vec![
                    RecoveredAuthorization::new_unchecked(
                        Authorization { chain_id: U256::ZERO, address: delegate, nonce: 0 },
                        RecoveredAuthority::Invalid,
                    ),
                    recovered_auth(live, delegate),
                ],
            ));
        let mut evm = ctx.build_base();
        let handler =
            BaseHandler::<_, EVMError<_, BaseTransactionError>, EthFrame<EthInterpreter>>::new();
        let mut init_and_floor_gas = InitialAndFloorGas::new(0, 0);
        handler
            .apply_eip7702_auth_list(&mut evm, &mut init_and_floor_gas)
            .expect("an invalid 7702 signature must skip, not fail the transaction");
        assert!(
            evm.ctx_mut()
                .journal_mut()
                .load_account_with_code(live)
                .unwrap()
                .info
                .code
                .as_ref()
                .is_some_and(|code| code.is_eip7702()),
            "a later valid authority must still apply after a skipped bad signature"
        );
    }

    #[test]
    fn cobalt_7702_mixed_auth_list_skips_only_revoked() {
        let live = Address::repeat_byte(0x11);
        let revoked = Address::repeat_byte(0x12);
        let delegate = Address::repeat_byte(0x44);
        let caller = Address::repeat_byte(0x33);
        let mut db = standard_keystore_db(live, None);
        db.insert_account_info(
            revoked,
            AccountInfo { balance: U256::from(1_000_000), ..Default::default() },
        );
        seed_account_state(&mut db, revoked, pack_inline_self(0, 0, true));

        let ctx = standard_keystore_context(db, BaseUpgrade::Cobalt).with_tx(standard_7702_tx(
            caller,
            vec![recovered_auth(live, delegate), recovered_auth(revoked, delegate)],
        ));
        let mut evm = ctx.build_base();
        let handler =
            BaseHandler::<_, EVMError<_, BaseTransactionError>, EthFrame<EthInterpreter>>::new();
        let mut init_and_floor_gas = InitialAndFloorGas::new(0, 0);
        handler
            .apply_eip7702_auth_list(&mut evm, &mut init_and_floor_gas)
            .expect("mixed 7702 list must not fail the transaction");

        let live_delegated = evm
            .ctx_mut()
            .journal_mut()
            .load_account_with_code(live)
            .unwrap()
            .info
            .code
            .as_ref()
            .is_some_and(|code| code.is_eip7702());
        let revoked_delegated = evm
            .ctx_mut()
            .journal_mut()
            .load_account_with_code(revoked)
            .unwrap()
            .info
            .code
            .as_ref()
            .is_some_and(|code| code.is_eip7702());
        assert!(live_delegated, "live EOA authority must still apply");
        assert!(!revoked_delegated, "revoked authority must be skipped");
    }

    #[test]
    fn pre_cobalt_7702_auth_skips_keystore() {
        let authority = Address::repeat_byte(0x11);
        assert!(
            authority_has_delegation(
                standard_keystore_db(authority, Some(pack_inline_self(0, 0, true))),
                BaseUpgrade::Isthmus,
                authority
            ),
            "pre-Cobalt 7702 auths must not consult the keystore"
        );
    }
}
