//! Handler related to Base chain
use alloc::{borrow::Cow, boxed::Box, vec::Vec};

use base_common_chains::BaseUpgrade;
use base_common_consensus::{
    AA_TX_TYPE_ID, ACCOUNT_CONFIG_ADDRESS, K1_VERIFIER_ADDRESS, NONCE_MANAGER_ADDRESS, OwnerScope,
    Predeploys, REVOKED_VERIFIER, TxContextValues, account_state_slot, implicit_eoa_owner_id,
    nonce_slot, owner_config_slot, parse_account_state, parse_owner_config, write_sequence,
};
use revm::{
    bytecode::Bytecode,
    context::{
        LocalContextTr,
        journaled_state::{JournalCheckpoint, account::JournaledAccountTr},
        result::InvalidTransaction,
    },
    context_interface::{
        Block, Cfg, ContextTr, Database, JournalTr, Transaction,
        context::ContextError,
        journaled_state::JournalLoadError,
        result::{EVMError, ExecutionResult, FromStringError},
    },
    handler::{
        EthFrame, EvmTr, FrameResult, Handler, MainnetHandler,
        evm::FrameTr,
        handler::EvmTrError,
        post_execution::{self, reimburse_caller},
        pre_execution::{calculate_caller_fee, validate_account_nonce_and_code_with_components},
    },
    inspector::{Inspector, InspectorEvmTr, InspectorHandler},
    interpreter::{
        CallInput, CallInputs, CallOutcome, CallScheme, CallValue, FrameInput, Gas,
        InitialAndFloorGas, InstructionResult, InterpreterResult, SharedMemory,
        interpreter::EthInterpreter, interpreter_action::FrameInit,
    },
    primitives::{Address, B256, Bytes, KECCAK_EMPTY, U256, hardfork::SpecId},
};

use crate::{
    BaseContextTr, BaseHaltReason, Eip8130Call, Eip8130Parts, L1BlockInfo,
    precompiles::{clear_eip8130_tx_context, set_eip8130_tx_context},
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

impl<EVM, ERROR, FRAME> BaseHandler<EVM, ERROR, FRAME>
where
    EVM: EvmTr<Context: BaseContextTr, Frame = FRAME>,
    ERROR: EvmTrError<EVM> + From<BaseTransactionError> + FromStringError + IsTxError,
    FRAME: FrameTr<FrameResult = FrameResult, FrameInit = FrameInit>,
{
    fn execute_eip8130_calls(
        &mut self,
        evm: &mut EVM,
        parts: &Eip8130Parts,
        gas_limit: u64,
    ) -> Result<FrameResult, ERROR> {
        let mut remaining_gas = gas_limit;
        let mut refunded_gas = 0;
        let mut any_call = false;
        let mut any_success = false;
        let mut last_result = InstructionResult::Return;
        let mut phase_statuses = Vec::with_capacity(parts.call_phases.len());

        for phase in &parts.call_phases {
            let mut phase_success = !phase.is_empty();
            for call in phase {
                any_call = true;
                let frame_input =
                    eip8130_call_frame::<EVM, ERROR>(evm, parts.sender, call, remaining_gas)?;
                let frame_result = <Self as Handler>::run_exec_loop(self, evm, frame_input)?;
                let interpreter_result = frame_result.interpreter_result();
                remaining_gas = interpreter_result.gas.remaining();
                refunded_gas += interpreter_result.gas.refunded();
                last_result = interpreter_result.result;

                if interpreter_result.result.is_ok() {
                    any_success = true;
                } else {
                    phase_success = false;
                }
            }

            if phase_success {
                any_success = true;
            }
            phase_statuses.push(phase_success);
        }

        let result = if any_success || !any_call { InstructionResult::Return } else { last_result };
        let output = phase_statuses.into_iter().map(u8::from).collect::<Vec<_>>().into();

        Ok(eip8130_aggregate_result(gas_limit, remaining_gas, refunded_gas, result, output))
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

        let is_aa = tx.tx_type() == AA_TX_TYPE_ID
            || tx
                .enveloped_tx()
                .and_then(|bytes| bytes.first())
                .is_some_and(|tx_type| *tx_type == AA_TX_TYPE_ID);
        let eip8130_parts = tx.eip8130_parts().cloned();
        if let Some(parts) = &eip8130_parts {
            set_eip8130_tx_context(Some(TxContextValues {
                sender: parts.sender,
                payer: parts.payer,
                owner_id: parts.owner_id,
                gas_limit: tx.gas_limit(),
                max_cost: U256::from(tx.gas_limit()) * U256::from(tx.max_fee_per_gas()),
                calls: parts
                    .call_phases
                    .iter()
                    .map(|phase| phase.iter().map(|call| (call.to, call.data.clone())).collect())
                    .collect(),
            }));
        } else {
            clear_eip8130_tx_context();
        }

        // L1 block info is stored in the context for later use.
        // and it will be reloaded from the database if it is not for the current block.
        if chain.l2_block != Some(block.number()) {
            *chain = L1BlockInfo::try_fetch(journal.db_mut(), block.number(), spec)?;
        }

        if let Some(parts) = &eip8130_parts {
            if let Some(error) = parts.auth_error {
                return Err(InvalidTransaction::Str(Cow::Owned(format!(
                    "invalid EIP-8130 auth: {error}"
                )))
                .into());
            }

            {
                let mut account_config = journal.load_account_mut(ACCOUNT_CONFIG_ADDRESS)?.data;
                let slot = owner_config_slot(parts.sender, parts.owner_id);
                let packed = match account_config.sload(slot.into(), false) {
                    Ok(state) => state.data.present_value(),
                    Err(JournalLoadError::ColdLoadSkipped) => U256::ZERO,
                    Err(JournalLoadError::DBError(err)) => {
                        return Err(ERROR::from_string(format!(
                            "failed to load AA owner config slot: {err}"
                        )));
                    }
                };
                let (verifier, scope) = parse_owner_config(B256::from(packed.to_be_bytes::<32>()));

                if verifier == REVOKED_VERIFIER {
                    return Err(InvalidTransaction::Str(Cow::Borrowed(
                        "EIP-8130 sender owner is revoked",
                    ))
                    .into());
                }
                if verifier == Address::ZERO {
                    if parts.sender_verifier != K1_VERIFIER_ADDRESS
                        || parts.owner_id != implicit_eoa_owner_id(parts.sender)
                    {
                        return Err(InvalidTransaction::Str(Cow::Borrowed(
                            "EIP-8130 sender owner is not authorized",
                        ))
                        .into());
                    }
                } else if verifier != parts.sender_verifier {
                    return Err(InvalidTransaction::Str(Cow::Borrowed(
                        "EIP-8130 sender verifier does not match owner config",
                    ))
                    .into());
                } else if scope != 0 && (scope & OwnerScope::SENDER) == 0 {
                    return Err(InvalidTransaction::Str(Cow::Borrowed(
                        "EIP-8130 sender owner lacks SENDER scope",
                    ))
                    .into());
                }
            }

            if !parts.config_writes.is_empty() || !parts.sequence_updates.is_empty() {
                let mut account_config = journal.load_account_mut(ACCOUNT_CONFIG_ADDRESS)?.data;
                let packed =
                    match account_config.sload(account_state_slot(parts.sender).into(), false) {
                        Ok(state) => state.data.present_value(),
                        Err(JournalLoadError::ColdLoadSkipped) => U256::ZERO,
                        Err(JournalLoadError::DBError(err)) => {
                            return Err(ERROR::from_string(format!(
                                "failed to load AA account state slot: {err}"
                            )));
                        }
                    };
                let account_state = parse_account_state(packed);
                if block.timestamp() < account_state.unlocks_at {
                    return Err(InvalidTransaction::Str(Cow::Borrowed(
                        "EIP-8130 account is locked",
                    ))
                    .into());
                }
            }
        }

        if is_aa {
            let nonce_key = tx.aa_nonce_key().unwrap_or_default();
            let slot = nonce_slot(tx.caller(), nonce_key);
            let tx_nonce = tx.nonce();
            let mut nonce_manager = journal.load_account_mut(NONCE_MANAGER_ADDRESS)?.data;
            let state_nonce = match nonce_manager.sload(slot.into(), false) {
                Ok(state) => state.data.present_value().to::<u64>(),
                Err(JournalLoadError::ColdLoadSkipped) => tx_nonce,
                Err(JournalLoadError::DBError(err)) => {
                    return Err(ERROR::from_string(format!("failed to load AA nonce slot: {err}")));
                }
            };

            if tx_nonce < state_nonce {
                return Err(
                    InvalidTransaction::NonceTooLow { tx: tx_nonce, state: state_nonce }.into()
                );
            }
            if tx_nonce > state_nonce {
                return Err(
                    InvalidTransaction::NonceTooHigh { tx: tx_nonce, state: state_nonce }.into()
                );
            }

            // Bump the 2D nonce lane at admission time so RPC `nonce_key` reads advance once
            // the AA transaction is accepted for execution.
            match nonce_manager.sstore(
                slot.into(),
                U256::from(state_nonce.saturating_add(1)),
                false,
            ) {
                Ok(_) | Err(JournalLoadError::ColdLoadSkipped) => {}
                Err(JournalLoadError::DBError(err)) => {
                    return Err(ERROR::from_string(format!(
                        "failed to write AA nonce slot: {err}"
                    )));
                }
            }
        }

        if let Some(parts) = &eip8130_parts {
            for write in parts.pre_writes.iter().chain(parts.config_writes.iter()) {
                let mut account = journal.load_account_mut(write.address)?.data;
                match account.sstore(write.slot.into(), write.value, false) {
                    Ok(_) | Err(JournalLoadError::ColdLoadSkipped) => {}
                    Err(JournalLoadError::DBError(err)) => {
                        return Err(ERROR::from_string(format!(
                            "failed to apply AA storage write: {err}"
                        )));
                    }
                }
            }

            for sequence in &parts.sequence_updates {
                let mut account = journal.load_account_mut(ACCOUNT_CONFIG_ADDRESS)?.data;
                let current = match account.sload(sequence.slot.into(), false) {
                    Ok(state) => state.data.present_value(),
                    Err(JournalLoadError::ColdLoadSkipped) => U256::ZERO,
                    Err(JournalLoadError::DBError(err)) => {
                        return Err(ERROR::from_string(format!(
                            "failed to load AA sequence slot: {err}"
                        )));
                    }
                };
                let updated = write_sequence(current, sequence.is_multichain, sequence.new_value);
                match account.sstore(sequence.slot.into(), updated, false) {
                    Ok(_) | Err(JournalLoadError::ColdLoadSkipped) => {}
                    Err(JournalLoadError::DBError(err)) => {
                        return Err(ERROR::from_string(format!(
                            "failed to write AA sequence slot: {err}"
                        )));
                    }
                }
            }

            for code_placement in &parts.code_placements {
                let mut account = journal.load_account_with_code_mut(code_placement.address)?.data;
                account.set_code_and_hash_slow(Bytecode::new_raw(code_placement.code.clone()));
            }

            if let Some(target) = parts.delegation_target {
                let mut account = journal.load_account_with_code_mut(parts.sender)?.data;
                account.set_code_and_hash_slow(delegation_code(target));
            } else {
                let mut account = journal.load_account_with_code_mut(parts.sender)?.data;
                if account.account().info.code_hash == KECCAK_EMPTY {
                    account.set_code_and_hash_slow(Bytecode::new_raw(
                        parts.auto_delegation_code.clone(),
                    ));
                }
            }
        }

        let fee_payer = eip8130_parts.as_ref().map_or(tx.caller(), |parts| parts.payer);
        let mut fee_payer_account = journal.load_account_with_code_mut(fee_payer)?.data;

        // EIP-8130 transactions use NonceManager lanes (nonce_key/nonce_sequence) instead of
        // the sender account nonce; skip legacy account-nonce validation in this compatibility path.
        if !is_aa {
            validate_account_nonce_and_code_with_components(
                &fee_payer_account.account().info,
                tx,
                cfg,
            )?;
        }

        // check additional cost and deduct it from the caller's balances
        let mut balance = fee_payer_account.account().info.balance;

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
        fee_payer_account.set_balance(balance);
        if tx.kind().is_call() && !is_aa {
            fee_payer_account.bump_nonce();
        }

        Ok(())
    }

    fn execution(
        &mut self,
        evm: &mut Self::Evm,
        init_and_floor_gas: &InitialAndFloorGas,
    ) -> Result<FrameResult, Self::Error> {
        let gas_limit = evm.ctx().tx().gas_limit() - init_and_floor_gas.initial_gas;
        let Some(parts) = evm.ctx().tx().eip8130_parts().cloned() else {
            let first_frame_input = self.first_frame_input(evm, gas_limit)?;
            let mut frame_result = self.run_exec_loop(evm, first_frame_input)?;
            self.last_frame_result(evm, &mut frame_result)?;
            return Ok(frame_result);
        };

        let mut frame_result = self.execute_eip8130_calls(evm, &parts, gas_limit)?;
        self.last_frame_result(evm, &mut frame_result)?;
        Ok(frame_result)
    }

    fn last_frame_result(
        &mut self,
        evm: &mut Self::Evm,
        frame_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        let ctx = evm.ctx();
        let tx = ctx.tx();
        let is_deposit = tx.tx_type() == DEPOSIT_TRANSACTION_TYPE;
        let tx_gas_limit = tx.gas_limit();
        let is_regolith = ctx.cfg().spec().is_enabled_in(BaseUpgrade::Regolith);

        let instruction_result = frame_result.interpreter_result().result;
        let gas = frame_result.gas_mut();
        let remaining = gas.remaining();
        let refunded = gas.refunded();

        // Spend the gas limit. Gas is reimbursed when the tx returns successfully.
        *gas = Gas::new_spent(tx_gas_limit);

        if instruction_result.is_ok() {
            if !is_deposit || is_regolith {
                gas.erase_cost(remaining);
                gas.record_refund(refunded);
            } else if is_deposit && tx.is_system_transaction() {
                gas.erase_cost(tx_gas_limit);
            }
        } else if instruction_result.is_revert() && (!is_deposit || is_regolith) {
            gas.erase_cost(remaining);
        }
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

        let aa_payer = evm.ctx().tx().eip8130_parts().map(|parts| parts.payer);
        if let Some(payer) = aa_payer {
            reimburse_account(evm.ctx(), payer, frame_result.gas(), additional_refund)
                .map_err(From::from)
        } else {
            reimburse_caller(evm.ctx(), frame_result.gas(), additional_refund).map_err(From::from)
        }
    }

    fn refund(
        &self,
        evm: &mut Self::Evm,
        frame_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
        eip7702_refund: i64,
    ) {
        frame_result.gas_mut().record_refund(eip7702_refund);

        let is_deposit = evm.ctx().tx().tx_type() == DEPOSIT_TRANSACTION_TYPE;
        let is_regolith = evm.ctx().cfg().spec().is_enabled_in(BaseUpgrade::Regolith);

        // Prior to Regolith, deposit transactions did not receive gas refunds.
        let is_gas_refund_disabled = is_deposit && !is_regolith;
        if !is_gas_refund_disabled {
            frame_result.gas_mut().set_final_refund(
                evm.ctx().cfg().spec().into_eth_spec().is_enabled_in(SpecId::LONDON),
            );
        }
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
        frame_result: <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        match core::mem::replace(evm.ctx().error(), Ok(())) {
            Err(ContextError::Db(e)) => return Err(e.into()),
            Err(ContextError::Custom(e)) => return Err(Self::Error::from_string(e)),
            Ok(_) => (),
        }

        let exec_result =
            post_execution::output(evm.ctx(), frame_result).map_haltreason(BaseHaltReason::Base);

        if exec_result.is_halt() {
            let is_deposit = evm.ctx().tx().tx_type() == DEPOSIT_TRANSACTION_TYPE;
            if is_deposit && evm.ctx().cfg().spec().is_enabled_in(BaseUpgrade::Regolith) {
                return Err(ERROR::from(BaseTransactionError::HaltedDepositPostRegolith));
            }
        }
        evm.ctx().journal_mut().commit_tx();
        evm.ctx().chain_mut().clear_tx_l1_cost();
        evm.ctx().local_mut().clear();
        evm.frame_stack().clear();
        clear_eip8130_tx_context();

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
            let spec = ctx.cfg().spec();
            let tx = ctx.tx();
            let caller = tx.caller();
            let mint = tx.mint();
            let is_system_tx = tx.is_system_transaction();
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

            let gas_used = if spec.is_enabled_in(BaseUpgrade::Regolith) || !is_system_tx {
                gas_limit
            } else {
                0
            };
            // clear the journal
            output = Ok(ExecutionResult::Halt { reason: BaseHaltReason::FailedDeposit, gas_used })
        }

        // do the cleanup
        evm.ctx().chain_mut().clear_tx_l1_cost();
        evm.ctx().local_mut().clear();
        evm.frame_stack().clear();
        clear_eip8130_tx_context();

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

fn eip8130_call_frame<EVM, ERROR>(
    evm: &mut EVM,
    caller: Address,
    call: &Eip8130Call,
    gas_limit: u64,
) -> Result<FrameInit, ERROR>
where
    EVM: EvmTr<Context: BaseContextTr>,
    ERROR: EvmTrError<EVM>,
{
    let memory = {
        let ctx = evm.ctx_mut();
        let mut memory = SharedMemory::new_with_buffer(ctx.local().shared_memory_buffer().clone());
        memory.set_memory_limit(ctx.cfg().memory_limit());
        memory
    };

    Ok(FrameInit {
        depth: 0,
        memory,
        frame_input: FrameInput::Call(Box::new(CallInputs {
            input: CallInput::Bytes(call.data.clone()),
            gas_limit,
            target_address: call.to,
            bytecode_address: call.to,
            known_bytecode: eip8130_known_bytecode::<EVM, ERROR>(evm, call.to)?,
            caller,
            value: CallValue::Transfer(call.value),
            scheme: CallScheme::Call,
            is_static: false,
            return_memory_offset: 0..0,
        })),
    })
}

fn eip8130_known_bytecode<EVM, ERROR>(
    evm: &mut EVM,
    address: Address,
) -> Result<Option<(B256, Bytecode)>, ERROR>
where
    EVM: EvmTr<Context: BaseContextTr>,
    ERROR: EvmTrError<EVM>,
{
    let delegated_address = {
        let account = &evm.ctx_mut().journal_mut().load_account_with_code(address)?.info;
        if let Some(Bytecode::Eip7702(eip7702_bytecode)) = &account.code {
            Some(eip7702_bytecode.delegated_address)
        } else {
            return Ok(Some((account.code_hash(), account.code.clone().unwrap_or_default())));
        }
    };

    if let Some(delegated_address) = delegated_address {
        let account = &evm.ctx_mut().journal_mut().load_account_with_code(delegated_address)?.info;
        return Ok(Some((account.code_hash(), account.code.clone().unwrap_or_default())));
    }

    Ok(None)
}

fn eip8130_aggregate_result(
    gas_limit: u64,
    remaining_gas: u64,
    refunded_gas: i64,
    result: InstructionResult,
    output: Bytes,
) -> FrameResult {
    let mut gas = Gas::new(gas_limit);
    let spent = gas_limit.saturating_sub(remaining_gas);
    let recorded = gas.record_cost(spent);
    debug_assert!(recorded);
    gas.record_refund(refunded_gas);

    FrameResult::Call(CallOutcome::new(InterpreterResult { result, output, gas }, 0..0))
}

fn reimburse_account<CTX: ContextTr>(
    context: &mut CTX,
    account: Address,
    gas: &Gas,
    additional_refund: U256,
) -> Result<(), <CTX::Db as Database>::Error> {
    let basefee = context.block().basefee() as u128;
    let effective_gas_price = context.tx().effective_gas_price(basefee);
    let refund = U256::from(
        effective_gas_price.saturating_mul((gas.remaining() + gas.refunded() as u64) as u128),
    ) + additional_refund;

    context.journal_mut().load_account_mut(account)?.incr_balance(refund);
    Ok(())
}

/// Builds an EIP-7702 delegation designation bytecode for an AA delegation change.
pub fn delegation_code(target: Address) -> Bytecode {
    if target.is_zero() {
        return Bytecode::default();
    }

    let mut code = Vec::with_capacity(23);
    code.extend_from_slice(&[0xef, 0x01, 0x00]);
    code.extend_from_slice(target.as_slice());
    Bytecode::new_raw(Bytes::from(code))
}

#[cfg(test)]
mod tests {

    use alloy_primitives::uint;
    use base_common_consensus::Predeploys;
    use revm::{
        bytecode::Bytecode,
        context::{BlockEnv, CfgEnv, Context, TxEnv},
        database::InMemoryDB,
        database_interface::EmptyDB,
        handler::{EthFrame, Handler},
        interpreter::{CallOutcome, InstructionResult, InterpreterResult},
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

        handler.last_frame_result(&mut evm, &mut exec_result).unwrap();
        handler.refund(&mut evm, &mut exec_result, 0);
        *exec_result.gas()
    }

    #[test]
    fn test_revert_gas() {
        let ctx = Context::base()
            .with_tx(BaseTransaction::builder().base(TxEnv::builder().gas_limit(100)).build_fill())
            .with_cfg(CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Bedrock)));

        let gas = call_last_frame_return(ctx, InstructionResult::Revert, Gas::new(90));
        assert_eq!(gas.remaining(), 90);
        assert_eq!(gas.spent(), 10);
        assert_eq!(gas.refunded(), 0);
    }

    #[test]
    fn test_consume_gas() {
        let ctx = Context::base()
            .with_tx(BaseTransaction::builder().base(TxEnv::builder().gas_limit(100)).build_fill())
            .with_cfg(CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Regolith)));

        let gas = call_last_frame_return(ctx, InstructionResult::Stop, Gas::new(90));
        assert_eq!(gas.remaining(), 90);
        assert_eq!(gas.spent(), 10);
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
        assert_eq!(gas.spent(), 10);
        assert_eq!(gas.refunded(), 2); // min(20, 10/5)

        let gas = call_last_frame_return(ctx, InstructionResult::Revert, ret_gas);
        assert_eq!(gas.remaining(), 90);
        assert_eq!(gas.spent(), 10);
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
            .with_cfg(CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Bedrock)));
        let gas = call_last_frame_return(ctx, InstructionResult::Stop, Gas::new(90));
        assert_eq!(gas.remaining(), 0);
        assert_eq!(gas.spent(), 100);
        assert_eq!(gas.refunded(), 0);
    }

    #[test]
    fn test_consume_gas_sys_deposit_tx() {
        let ctx = Context::base()
            .with_tx(
                BaseTransaction::builder()
                    .base(TxEnv::builder().gas_limit(100))
                    .source_hash(B256::from([1u8; 32]))
                    .is_system_transaction()
                    .build_fill(),
            )
            .with_cfg(CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Bedrock)));
        let gas = call_last_frame_return(ctx, InstructionResult::Stop, Gas::new(90));
        assert_eq!(gas.remaining(), 100);
        assert_eq!(gas.spent(), 0);
        assert_eq!(gas.refunded(), 0);
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
        handler.validate_against_state_and_deduct_caller(&mut evm).unwrap();

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
        handler.validate_against_state_and_deduct_caller(&mut evm).unwrap();

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
        handler.validate_against_state_and_deduct_caller(&mut evm).unwrap();

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
}
