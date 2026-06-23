//! Enshrined EIP-8130 (account-abstraction) transaction execution.
//!
//! [`Eip8130Executor`] runs the EIP-8130 *pre-call* pipeline directly against the
//! block-execution journal, around (not inside) an EVM call frame: it authorizes
//! the sender/payer and account-configuration changes, validates and advances the
//! transaction's 2D nonce, charges the EIP-8130 intrinsic gas schedule, validates
//! the fee caps and debits the payer, and applies the transaction's account
//! changes (config changes, account creation, and delegation) — installing the
//! deferred account-*code* effects (created bytecode, delegation indicators, and
//! code-less-EOA auto-delegation).
//!
//! All storage access goes through a gas-free [`JournalStorageProvider`], so the
//! enshrined schedule is the single source of gas accounting (no EIP-2929/2200
//! double-counting). The executor is invoked from
//! [`BaseEvm::transact_raw`](crate::BaseEvm) when the transaction is an EIP-8130
//! transaction, bypassing the mainnet single-frame handler.
//!
//! # Scope
//!
//! This stage handles *account-management* transactions only: those whose `calls`
//! list is empty (config-change, account-create, and delegation transactions). A
//! transaction carrying calls is rejected here until phased call execution lands.
//! Fee settlement routes the base-fee portion to the base-fee vault and the
//! priority portion to the block beneficiary; L1 and operator fee routing are
//! deferred to the fee-settlement stage.

use alloc::vec::Vec;

use alloy_evm::{Database as AlloyDatabase, EvmInternals};
use alloy_primitives::{Address, Bytes, U256};
use base_common_consensus::{Eip8130Constants, Eip8130Contracts, Predeploys};
use base_common_precompiles::NonceManagerStorage;
use base_execution_eip8130::{
    AccountChangeApplier, AccountConfigurationStorage, FeeCheck, IntrinsicGas, IntrinsicGasInput,
    NonceMode, NonceValidator, TransactionAuthorizer,
};
use base_precompile_storage::{JournalStorageProvider, StorageCtx};
use revm::{
    context::{BlockEnv, TxEnv, journaled_state::account::JournaledAccountTr},
    context_interface::{
        Block, Cfg, ContextTr, JournalTr,
        result::{EVMError, ExecutionResult, Output, ResultGas, SuccessReason},
    },
    state::Bytecode,
};

use crate::{
    BaseContext, BaseContextTr, BaseHaltReason, BaseTransaction, BaseTransactionError, BaseTxTr,
};

/// The settled outcome of the EIP-8130 pre-call pipeline: the resolved actors,
/// the gas charged, and the fee split to apply against the journal.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Eip8130Outcome {
    /// The resolved transaction sender.
    pub sender: Address,
    /// The resolved gas payer (the sender, for self-pay).
    pub payer: Address,
    /// Total intrinsic gas charged for the transaction.
    pub gas_used: u64,
    /// Total ETH debited from the payer (`gas_used · effective_gas_price`).
    pub fee: U256,
    /// Base-fee portion of [`Self::fee`], routed to the base-fee vault.
    pub base_fee_amount: U256,
    /// Priority portion of [`Self::fee`], routed to the block beneficiary.
    pub priority_amount: U256,
    /// Whether the sender's protocol (basic) account nonce must be bumped
    /// (`nonce_key == 0`).
    pub bump_protocol_nonce: bool,
}

/// Executes enshrined EIP-8130 transactions against the block-execution journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Eip8130Executor;

impl Eip8130Executor {
    /// Executes the EIP-8130 transaction currently set on `ctx`, mutating the
    /// journal in place and returning a success [`ExecutionResult`]. Any
    /// validation failure reverts all journal writes via a checkpoint and
    /// surfaces a transaction error (the transaction is not included).
    pub fn execute<DB>(
        ctx: &mut BaseContext<DB>,
    ) -> Result<ExecutionResult<BaseHaltReason>, EVMError<DB::Error, BaseTransactionError>>
    where
        DB: AlloyDatabase,
        BaseContext<DB>: BaseContextTr
            + ContextTr<
                Db = DB,
                Tx = BaseTransaction<TxEnv>,
                Block = BlockEnv,
                Journal: core::fmt::Debug,
            >,
    {
        // The signed envelope is cloned out of the context because the pipeline
        // borrows `ctx` mutably (journal/account access) while needing the
        // envelope's fields throughout. The clone deep-copies the account-change
        // and call vectors; the auth blobs are ref-counted `Bytes`.
        let signed = ctx
            .tx()
            .eip8130_parts()
            .ok_or_else(|| {
                BaseTransactionError::eip8130("transaction is not an EIP-8130 transaction")
            })?
            .signed
            .clone();

        // Phased call execution is not yet supported; reject any call-bearing
        // transaction so nothing mis-executes against the placeholder TxEnv.
        if signed.tx().calls.iter().any(|phase| !phase.is_empty()) {
            return Err(BaseTransactionError::eip8130(
                "EIP-8130 phased call execution is not yet supported",
            )
            .into());
        }

        let chain_id = ctx.cfg().chain_id();
        // Consensus-critical: a clamped timestamp would silently shift the expiry
        // validation in the authorizer and nonce validator, so reject rather than
        // saturate. Block timestamps never approach `u64::MAX` in practice.
        let now: u64 = ctx
            .block()
            .timestamp()
            .try_into()
            .map_err(|_| BaseTransactionError::eip8130("block timestamp exceeds u64"))?;
        let base_fee: u128 = u128::from(ctx.block().basefee());
        let beneficiary = ctx.block().beneficiary();
        // Reuse the original wire bytes captured during `from_encoded_tx` instead
        // of re-encoding: this avoids an allocation and the assumption that
        // re-encoding is byte-identical, which matters because the EIP-8130
        // intrinsic-gas schedule meters the transaction size from these bytes.
        let encoded =
            ctx.tx().enveloped_tx().cloned().ok_or_else(|| {
                BaseTransactionError::eip8130("missing enveloped transaction bytes")
            })?;

        // Guard every journal write so a late-stage rejection discards the whole
        // transaction's mutations.
        let checkpoint = ctx.journal_mut().checkpoint();

        let outcome =
            match Self::authorize_and_apply(ctx, &signed, &encoded, chain_id, now, base_fee) {
                Ok(outcome) => outcome,
                Err(err) => {
                    ctx.journal_mut().checkpoint_revert(checkpoint);
                    return Err(err.into());
                }
            };

        if let Err(err) = Self::settle(ctx, &outcome, beneficiary) {
            ctx.journal_mut().checkpoint_revert(checkpoint);
            return Err(err);
        }

        // Explicitly close the checkpoint opened above before committing the
        // transaction. `commit_tx` subsumes open checkpoints today, but pairing
        // every `checkpoint` with a `checkpoint_commit`/`checkpoint_revert`
        // keeps the lifecycle unambiguous and robust to journal changes.
        ctx.journal_mut().checkpoint_commit();
        ctx.journal_mut().commit_tx();

        Ok(ExecutionResult::Success {
            reason: SuccessReason::Return,
            gas: ResultGas::new_with_state_gas(outcome.gas_used, 0, 0, 0),
            logs: Vec::new(),
            output: Output::Call(Bytes::new()),
        })
    }

    /// Runs the storage-backed pipeline (authorize, nonce, intrinsic gas, fee
    /// check, account-change apply) over a gas-free journal view, returning the
    /// settlement to apply. Storage writes land on the journal directly; the
    /// caller's checkpoint reverts them on error.
    fn authorize_and_apply<DB>(
        ctx: &mut BaseContext<DB>,
        signed: &base_common_consensus::Eip8130Signed,
        encoded: &[u8],
        chain_id: u64,
        now: u64,
        base_fee: u128,
    ) -> Result<Eip8130Outcome, BaseTransactionError>
    where
        DB: AlloyDatabase,
        BaseContext<DB>: BaseContextTr
            + ContextTr<
                Db = DB,
                Tx = BaseTransaction<TxEnv>,
                Block = BlockEnv,
                Journal: core::fmt::Debug,
            >,
    {
        let tx = signed.tx();
        let sender_is_eoa = tx.sender.is_none();
        let nonce_key = tx.nonce_key;
        let gas_limit = tx.gas_limit;
        let max_fee = tx.max_fee_per_gas;
        let max_priority = tx.max_priority_fee_per_gas;
        let expiry = tx.expiry;
        let sender_sig_hash = tx.sender_signature_hash();

        let internals = EvmInternals::from_context(ctx);
        let mut provider = JournalStorageProvider::new(internals, Address::ZERO);

        StorageCtx::enter(&mut provider, |sctx| {
            let acc = AccountConfigurationStorage::new(sctx);
            let mut nonce_mgr = NonceManagerStorage::new(sctx);

            // 1. Authorize sender, payer, and config changes.
            let authorized = TransactionAuthorizer::authorize(signed, &acc, chain_id, now)
                .map_err(BaseTransactionError::eip8130)?;
            let sender = authorized.actors.sender.account;
            let payer = authorized.actors.payer.as_ref().map_or(sender, |p| p.account);

            // 2. Resolve the nonce channel's first-use flag and validate the nonce.
            let protocol_nonce = sctx
                .with_account_info(sender, |info| Ok(info.nonce))
                .map_err(BaseTransactionError::eip8130)?;
            let nonce_key_first_use = if nonce_key == Eip8130Constants::NONCE_KEY_MAX {
                false
            } else if nonce_key == U256::ZERO {
                protocol_nonce == 0
            } else {
                nonce_mgr.get_nonce(sender, nonce_key).map_err(BaseTransactionError::eip8130)? == 0
            };
            NonceValidator::validate(
                tx,
                sender,
                sender_sig_hash,
                protocol_nonce,
                &nonce_mgr,
                NonceMode::Inclusion,
                now,
            )
            .map_err(BaseTransactionError::eip8130)?;

            // 3. Advance the nonce. The protocol (basic-account) nonce is bumped
            //    in `settle`; channel and expiring nonces are journal storage.
            let bump_protocol_nonce = if nonce_key == Eip8130Constants::NONCE_KEY_MAX {
                let replay = NonceValidator::replay_hash(sender, sender_sig_hash);
                nonce_mgr
                    .check_and_mark_expiring_nonce(replay, expiry)
                    .map_err(BaseTransactionError::eip8130)?;
                false
            } else if nonce_key == U256::ZERO {
                true
            } else {
                nonce_mgr
                    .increment_nonce(sender, nonce_key)
                    .map_err(BaseTransactionError::eip8130)?;
                false
            };

            // 4. Apply account changes and install the deferred code effects.
            let mut acc_mut = AccountConfigurationStorage::new(sctx);
            let applied = AccountChangeApplier::apply(signed, &mut acc_mut, sender)
                .map_err(BaseTransactionError::eip8130)?;
            if let Some(created) = &applied.created {
                sctx.set_code(created.address, Bytecode::new_raw(created.code.clone()))
                    .map_err(BaseTransactionError::eip8130)?;
            }
            if let Some(delegation) = &applied.delegation {
                let code = if delegation.target.is_zero() {
                    Bytecode::default()
                } else {
                    Bytecode::new_eip7702(delegation.target)
                };
                sctx.set_code(delegation.account, code).map_err(BaseTransactionError::eip8130)?;
            }

            // 5. Auto-delegate a code-less EOA sender to the default account so a
            // basic account can dispatch its calls. This is unconditional for
            // code-less EOA senders: an explicit delegation applied in step 4 with
            // a non-zero target leaves non-empty code and is preserved here, but
            // clearing the sender's delegation in the same transaction leaves it
            // code-less and is intentionally re-delegated — a basic-account sender
            // is always delegated to `DEFAULT_ACCOUNT`.
            let sender_auto_delegated = if sender_is_eoa
                && sctx
                    .with_account_info(sender, |info| Ok(info.is_empty_code_hash()))
                    .map_err(BaseTransactionError::eip8130)?
            {
                sctx.set_code(sender, Bytecode::new_eip7702(Eip8130Contracts::DEFAULT_ACCOUNT))
                    .map_err(BaseTransactionError::eip8130)?;
                true
            } else {
                false
            };

            // 6. Intrinsic gas under the EIP-8130 schedule.
            let intrinsic = IntrinsicGas::compute(
                signed,
                encoded,
                &IntrinsicGasInput::new(nonce_key_first_use, sender_auto_delegated),
            )
            .map_err(BaseTransactionError::eip8130)?;
            if intrinsic.execution_gas_available(gas_limit).is_none() {
                return Err(BaseTransactionError::eip8130(
                    "EIP-8130 sender-intrinsic gas exceeds the gas limit",
                ));
            }
            let gas_used = intrinsic.total();
            let payer_auth_cost = intrinsic.payer_auth;

            // 7. Fee caps and payer balance, then the fee split.
            FeeCheck::validate_fees(max_fee, max_priority, base_fee)
                .map_err(BaseTransactionError::eip8130)?;
            let payer_balance = sctx
                .with_account_info(payer, |info| Ok(info.balance))
                .map_err(BaseTransactionError::eip8130)?;
            FeeCheck::validate_balance(payer_balance, gas_limit, payer_auth_cost, max_fee)
                .map_err(BaseTransactionError::eip8130)?;

            // Consensus-critical: settle these with checked arithmetic so a
            // (practically impossible) overflow rejects the transaction rather
            // than silently clamping the fee or the split.
            let effective = FeeCheck::effective_gas_price(max_fee, max_priority, base_fee);
            let fee = U256::from(gas_used)
                .checked_mul(U256::from(effective))
                .ok_or_else(|| BaseTransactionError::eip8130("EIP-8130 fee overflow"))?;
            let base_fee_amount =
                U256::from(gas_used).checked_mul(U256::from(base_fee)).ok_or_else(|| {
                    BaseTransactionError::eip8130("EIP-8130 base-fee amount overflow")
                })?;
            let priority_amount = fee.checked_sub(base_fee_amount).ok_or_else(|| {
                BaseTransactionError::eip8130("EIP-8130 priority amount underflow")
            })?;

            Ok(Eip8130Outcome {
                sender,
                payer,
                gas_used,
                fee,
                base_fee_amount,
                priority_amount,
                bump_protocol_nonce,
            })
        })
    }

    /// Applies the balance and protocol-nonce mutations of a settled outcome to
    /// the journal: bumps the sender's basic nonce (if a protocol-nonce tx),
    /// debits the payer, and routes the fee split.
    fn settle<DB>(
        ctx: &mut BaseContext<DB>,
        outcome: &Eip8130Outcome,
        beneficiary: Address,
    ) -> Result<(), EVMError<DB::Error, BaseTransactionError>>
    where
        DB: AlloyDatabase,
        BaseContext<DB>: BaseContextTr
            + ContextTr<
                Db = DB,
                Tx = BaseTransaction<TxEnv>,
                Block = BlockEnv,
                Journal: core::fmt::Debug,
            >,
    {
        if outcome.bump_protocol_nonce {
            let mut sender_acc =
                ctx.journal_mut().load_account_mut(outcome.sender).map_err(EVMError::Database)?;
            sender_acc.bump_nonce();
        }

        {
            // Only the payer's balance is touched here, so load the account
            // without its bytecode (avoids an unnecessary code fetch from the DB).
            let mut payer_acc =
                ctx.journal_mut().load_account_mut(outcome.payer).map_err(EVMError::Database)?;
            let balance = payer_acc.balance();
            // `FeeCheck::validate_balance` guarantees the payer can cover the
            // fee; `checked_sub` turns any future gap in that invariant into an
            // explicit rejection rather than silently minting ETH (the vault and
            // beneficiary are credited the full split below).
            let debited = balance.checked_sub(outcome.fee).ok_or_else(|| {
                EVMError::Transaction(BaseTransactionError::eip8130(
                    "payer balance is below the settled fee",
                ))
            })?;
            payer_acc.set_balance(debited);
        }

        ctx.journal_mut()
            .balance_incr(Predeploys::BASE_FEE_VAULT, outcome.base_fee_amount)
            .map_err(EVMError::Database)?;
        ctx.journal_mut()
            .balance_incr(beneficiary, outcome.priority_amount)
            .map_err(EVMError::Database)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_evm::{Evm, FromTxWithEncoded, precompiles::PrecompilesMap};
    use alloy_primitives::{Address, B256, Bytes, U256, address, keccak256};
    use base_common_consensus::{BaseTxEnvelope, Call, Eip8130Signed, Predeploys, TxEip8130};
    use k256::ecdsa::SigningKey;
    use revm::{
        context::{BlockEnv, CfgEnv, Context},
        database::InMemoryDB,
        inspector::NoOpInspector,
        state::AccountInfo,
    };

    use super::*;
    use crate::{BaseEvm, BaseSpecId, BaseTransaction, BaseUpgrade, Builder, DefaultBase};

    const CHAIN_ID: u64 = 8453;
    const NOW: u64 = 1_000;
    const BASE_FEE: u64 = 1_000_000_000;
    const BENEFICIARY: Address = address!("0x00000000000000000000000000000000000000bb");

    fn signing_key(byte: u8) -> SigningKey {
        SigningKey::from_slice(&[byte; 32]).unwrap()
    }

    fn eoa_address(key: &SigningKey) -> Address {
        let point = key.verifying_key().to_encoded_point(false);
        Address::from_slice(&keccak256(&point.as_bytes()[1..])[12..])
    }

    /// 65-byte `r || s || v` signature (`v` in `{27, 28}`, low-s) over `hash`.
    fn eoa_sig(key: &SigningKey, hash: B256) -> Bytes {
        let (signature, recid) = key.sign_prehash_recoverable(hash.as_slice()).unwrap();
        let mut out = vec![0u8; 65];
        out[..64].copy_from_slice(&signature.to_bytes());
        out[64] = recid.to_byte() + 27;
        Bytes::from(out)
    }

    fn base_tx() -> TxEip8130 {
        TxEip8130 {
            chain_id: CHAIN_ID,
            sender: None,
            nonce_key: U256::ZERO,
            nonce_sequence: 0,
            expiry: 0,
            max_priority_fee_per_gas: 1_000_000_000,
            max_fee_per_gas: 5_000_000_000,
            gas_limit: 1_000_000,
            account_changes: Vec::new(),
            calls: Vec::new(),
            metadata: Bytes::new(),
            payer: None,
        }
    }

    fn eoa_signed(tx: TxEip8130, key: &SigningKey) -> Eip8130Signed {
        let hash = tx.sender_signature_hash();
        Eip8130Signed::new(tx, eoa_sig(key, hash), Bytes::new())
    }

    fn into_base_tx(signed: &Eip8130Signed) -> BaseTransaction<revm::context::TxEnv> {
        let envelope = BaseTxEnvelope::Eip8130(signed.clone());
        let encoded: Bytes = alloy_eips::eip2718::Encodable2718::encoded_2718(&envelope).into();
        BaseTransaction::from_encoded_tx(&envelope, Address::ZERO, encoded)
    }

    fn evm_with(
        balance: U256,
        sender: Address,
    ) -> BaseEvm<InMemoryDB, NoOpInspector, PrecompilesMap> {
        let mut db = InMemoryDB::default();
        db.insert_account_info(sender, AccountInfo { balance, ..Default::default() });
        Context::base()
            .with_db(db)
            .with_cfg(
                CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Isthmus))
                    .with_chain_id(CHAIN_ID),
            )
            .with_block(BlockEnv {
                number: U256::from(1u64),
                timestamp: U256::from(NOW),
                basefee: BASE_FEE,
                beneficiary: BENEFICIARY,
                ..Default::default()
            })
            .build_with_inspector(NoOpInspector)
    }

    #[test]
    fn call_bearing_eip8130_transaction_is_rejected() {
        let key = signing_key(0x11);
        let mut tx = base_tx();
        tx.calls = vec![vec![Call { to: Address::with_last_byte(0x42), data: Bytes::new() }]];
        let signed = eoa_signed(tx, &key);
        let sender = eoa_address(&key);

        let mut evm = evm_with(U256::from(10u64).pow(U256::from(18u64)), sender);
        let err = evm.transact_raw(into_base_tx(&signed)).unwrap_err();
        let EVMError::Transaction(BaseTransactionError::Eip8130(reason)) = err else {
            panic!("expected an EIP-8130 transaction rejection, got {err:?}");
        };
        assert!(reason.contains("call execution"), "unexpected reason: {reason}");
    }

    #[test]
    fn eoa_self_pay_transaction_executes_and_charges_sender() {
        let key = signing_key(0x22);
        let sender = eoa_address(&key);
        let signed = eoa_signed(base_tx(), &key);

        let initial_balance = U256::from(10u64).pow(U256::from(18u64));
        let mut evm = evm_with(initial_balance, sender);
        let outcome = evm.transact_raw(into_base_tx(&signed)).expect("8130 tx should execute");

        let result = outcome.result;
        assert!(result.is_success(), "expected success, got {result:?}");
        assert!(result.gas().tx_gas_used() > 0);

        // The sender's protocol nonce was bumped and a code-less EOA sender was
        // auto-delegated to the default account.
        let sender_acc = outcome.state.get(&sender).expect("sender in state");
        assert_eq!(sender_acc.info.nonce, 1);
        assert!(!sender_acc.info.is_empty_code_hash(), "sender should be auto-delegated");
        assert!(sender_acc.info.balance < initial_balance, "payer should be debited");

        // Fees were routed: base fee to the vault, priority tip to the beneficiary.
        assert!(outcome.state.contains_key(&Predeploys::BASE_FEE_VAULT));
        assert!(outcome.state.contains_key(&BENEFICIARY));
    }

    #[test]
    fn underfunded_payer_is_rejected() {
        let key = signing_key(0x33);
        let sender = eoa_address(&key);
        let signed = eoa_signed(base_tx(), &key);

        // Far below the worst-case charge (gas_limit · max_fee_per_gas).
        let mut evm = evm_with(U256::from(1_000u64), sender);
        let err = evm.transact_raw(into_base_tx(&signed)).unwrap_err();
        assert!(matches!(err, EVMError::Transaction(BaseTransactionError::Eip8130(_))));
    }
}
