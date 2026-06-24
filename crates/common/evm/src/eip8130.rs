//! Enshrined EIP-8130 (account-abstraction) transaction execution.
//!
//! [`Eip8130Executor`] runs the full EIP-8130 transaction directly against the
//! block-execution journal. The *pre-call* pipeline runs around (not inside) an
//! EVM call frame: it authorizes the sender/payer and account-configuration
//! changes, validates and advances the transaction's 2D nonce, charges the
//! EIP-8130 intrinsic gas schedule, validates the fee caps, applies the
//! transaction's account changes (config changes, account creation, and
//! delegation) — installing the deferred account-*code* effects — and
//! pre-charges the gas payer. It then publishes the [transaction context]
//! (sender / payer / actor id) and dispatches the transaction's `calls` as real
//! EVM call frames, settling the final fee and refunding unused gas afterwards.
//!
//! Pre-call storage access goes through a gas-free [`JournalStorageProvider`], so
//! the enshrined schedule is the single source of gas accounting for the pre-call
//! work (no EIP-2929/2200 double-counting); the `calls` themselves are metered by
//! the EVM under the standard gas rules, drawing from a single pool of
//! `gas_limit - sender_intrinsic_gas`. The executor is invoked from
//! [`BaseEvm::transact_raw`] when the transaction is an EIP-8130 transaction,
//! bypassing the mainnet single-frame handler.
//!
//! # Call execution
//!
//! `calls` is a two-level structure (`Vec<Vec<Call>>`): an ordered list of
//! **phases**, each an ordered list of calls. Phases draw from a single gas pool
//! and commit independently in sequence; the calls within a phase are atomic
//! (all-or-nothing). If any call in a phase reverts (or is blocked by the policy
//! gate), that phase's state changes are discarded and every later phase is
//! skipped, but the gas already consumed is still charged and the transaction is
//! still included (nonce consumed, fee paid). Each call is dispatched from
//! `sender` to `call.to` with `msg.value == 0` and `tx.origin == sender`.
//!
//! # Scope
//!
//! Per-phase receipt status (`phaseStatuses`) and protocol-injected
//! account-change logs are not yet surfaced; the overall transaction status
//! (all-phases-succeeded vs reverted) is reported through the returned
//! [`ExecutionResult`] variant ([`ExecutionResult::Success`] vs
//! [`ExecutionResult::Revert`]).
//!
//! [transaction context]: TxContextStorage
//! [`BaseEvm::transact_raw`]: crate::BaseEvm

use alloc::{boxed::Box, rc::Rc, vec::Vec};

use alloy_evm::{Database as AlloyDatabase, EvmInternals};
use alloy_primitives::{Address, B256, Bytes, U256};
use base_common_consensus::{Eip8130Constants, Eip8130Contracts, Predeploys};
use base_common_precompiles::{NonceManagerStorage, TxContextStorage};
use base_execution_eip8130::{
    AccountChangeApplier, AccountConfigurationStorage, FeeCheck, IntrinsicGas, IntrinsicGasInput,
    NonceMode, NonceValidator, TransactionAuthorizer,
};
use base_precompile_storage::{JournalStorageProvider, StorageCtx};
use revm::{
    context::{BlockEnv, LocalContextTr, TxEnv, journaled_state::account::JournaledAccountTr},
    context_interface::{
        Block, Cfg, ContextTr, JournalTr,
        result::{EVMError, ExecutionResult, Output, ResultGas, SuccessReason},
    },
    handler::{EthFrame, EvmTr, FrameResult, Handler, PrecompileProvider},
    interpreter::{
        CallInput, CallInputs, CallScheme, CallValue, FrameInput, InterpreterResult, SharedMemory,
        interpreter::EthInterpreter, interpreter_action::FrameInit,
    },
    state::Bytecode,
};

use crate::{
    BaseContext, BaseContextTr, BaseEvm, BaseHaltReason, BaseSpecId, BaseTransaction,
    BaseTransactionError, BaseTxTr, L1BlockInfo, handler::BaseHandler,
};

/// EIP-3529 maximum gas refund quotient: refunds are capped at `gas_used / 5`.
/// Base is post-London, so this is constant across all live specs.
const MAX_REFUND_QUOTIENT: u64 = 5;

/// The resolved pre-call context of an EIP-8130 transaction: the authorized
/// actors, the policy gate target, and the gas/fee parameters needed to dispatch
/// `calls` and settle the fee.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Eip8130Outcome {
    /// The resolved transaction sender (the account dispatching `calls`).
    pub sender: Address,
    /// The resolved gas payer (the sender, for self-pay).
    pub payer: Address,
    /// The authenticated sender actor's id (published to the transaction context
    /// and used as the policy-gate subject).
    pub sender_actor_id: B256,
    /// The sender actor's policy type; `0` means ungated (no policy gate).
    pub policy_type: u8,
    /// The policy gate target (`policy_manager(sender, actorId)`) resolved once
    /// at authorization; every `call.to` must equal this when `policy_type != 0`.
    pub policy_target: Address,
    /// The transaction's `gas_limit` (the sender-signed budget for sender
    /// authentication, intrinsic costs, account changes, and call execution).
    pub gas_limit: u64,
    /// Sender-intrinsic gas (intrinsic gas excluding payer authentication).
    pub sender_intrinsic: u64,
    /// Payer-authentication gas, metered on top of `gas_limit`.
    pub payer_auth: u64,
    /// Gas available to `calls` (`gas_limit - sender_intrinsic`).
    pub execution_gas_available: u64,
    /// EIP-1559 effective gas price for the transaction.
    pub effective: u128,
    /// Block base fee per gas.
    pub base_fee: u128,
    /// Whether the sender's protocol (basic) account nonce must be bumped
    /// (`nonce_key == 0`).
    pub bump_protocol_nonce: bool,
}

/// The result of dispatching an EIP-8130 transaction's `calls`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CallsResult {
    /// Total regular gas consumed by dispatched calls (across all phases,
    /// including the gas spent by a reverting phase up to its revert).
    call_gas_spent: u64,
    /// Net gas refund accumulated across all committed (successful) phases,
    /// pre-cap. This is the standard transaction-level refund counter: every
    /// call's `Gas::refunded()` (which may be negative — e.g. re-dirtying a slot
    /// a prior call cleared) is summed signed across the whole transaction, so
    /// offsetting SSTORE refunds cancel exactly as they would under a single
    /// continuous EVM execution. It is clamped to `>= 0` and capped per
    /// EIP-3529 only once, in [`Eip8130Executor::settle_fees`].
    refund: i64,
    /// `true` if any phase reverted (or was blocked by the policy gate); later
    /// phases are then skipped.
    reverted: bool,
    /// The return data of the call that reverted the transaction (or the
    /// `ActorPolicyViolation` payload for a policy-gate block); empty on success.
    output: Bytes,
}

/// Executes enshrined EIP-8130 transactions against the block-execution journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Eip8130Executor;

impl Eip8130Executor {
    /// Executes the EIP-8130 transaction currently set on `evm`, mutating the
    /// journal in place and returning the [`ExecutionResult`]. A success result
    /// is returned for an included transaction whether or not its `calls`
    /// reverted ([`ExecutionResult::Revert`] reports a phase revert); only a
    /// *validity* failure surfaces an [`EVMError`], reverting all journal writes
    /// via a checkpoint so the transaction is not included.
    pub fn execute<DB, I, P>(
        evm: &mut BaseEvm<DB, I, P>,
    ) -> Result<ExecutionResult<BaseHaltReason>, EVMError<DB::Error, BaseTransactionError>>
    where
        DB: AlloyDatabase,
        P: PrecompileProvider<BaseContext<DB>, Output = InterpreterResult>,
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
        let signed = evm
            .ctx()
            .tx()
            .eip8130_parts()
            .ok_or_else(|| {
                BaseTransactionError::eip8130("transaction is not an EIP-8130 transaction")
            })?
            .signed
            .clone();

        let ctx = evm.ctx_mut();
        let chain_id = ctx.cfg().chain_id();
        let spec = ctx.cfg().spec();
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
        let block_number = ctx.block().number();
        // Reuse the original wire bytes captured during `from_encoded_tx` instead
        // of re-encoding: this avoids an allocation and the assumption that
        // re-encoding is byte-identical, which matters because the EIP-8130
        // intrinsic-gas schedule meters the transaction size from these bytes.
        let encoded =
            ctx.tx().enveloped_tx().cloned().ok_or_else(|| {
                BaseTransactionError::eip8130("missing enveloped transaction bytes")
            })?;

        // Refresh the cached L1 block info for this block so the L1 and operator
        // fee components route correctly (the mainnet handler does this in
        // `validate_against_state_and_deduct_caller`; the 8130 path bypasses it).
        if ctx.chain().l2_block != Some(block_number) {
            let fetched = L1BlockInfo::try_fetch(ctx.journal_mut().db_mut(), block_number, spec)
                .map_err(EVMError::Database)?;
            *ctx.chain_mut() = fetched;
        }

        // Guard every journal write so a validity rejection discards the whole
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

        // Pre-charge the payer the worst-case fee (so `calls` cannot spend the
        // gas reservation), publish the transaction context, and run `calls`.
        let prepay = match Self::prepay(ctx, &outcome, &encoded, spec) {
            Ok(prepay) => prepay,
            Err(err) => {
                ctx.journal_mut().checkpoint_revert(checkpoint);
                // `prepay` may have cached this tx's L1 cost; clear it so the next
                // transaction in the block recomputes (parity with the mainnet
                // handler's `catch_error` cleanup).
                ctx.chain_mut().clear_tx_l1_cost();
                return Err(err);
            }
        };

        let calls = match Self::execute_calls(evm, &signed, &outcome) {
            Ok(calls) => calls,
            Err(err) => {
                let ctx = evm.ctx_mut();
                ctx.journal_mut().checkpoint_revert(checkpoint);
                ctx.chain_mut().clear_tx_l1_cost();
                return Err(err);
            }
        };

        let ctx = evm.ctx_mut();
        let gas_used =
            match Self::settle_fees(ctx, &outcome, &calls, prepay, &encoded, spec, beneficiary) {
                Ok(gas_used) => gas_used,
                Err(err) => {
                    ctx.journal_mut().checkpoint_revert(checkpoint);
                    ctx.chain_mut().clear_tx_l1_cost();
                    return Err(err);
                }
            };

        let logs = ctx.journal_mut().take_logs();

        // Explicitly close the checkpoint opened above before committing the
        // transaction, keeping the checkpoint lifecycle unambiguous.
        ctx.journal_mut().checkpoint_commit();
        ctx.journal_mut().commit_tx();
        ctx.chain_mut().clear_tx_l1_cost();

        // Parity with the mainnet handler's post-commit cleanup: reclaim the
        // `LocalContext` shared-memory buffer and drain the frame stack so no
        // stale state leaks into the next transaction when the `BaseEvm` is
        // reused across a block.
        evm.ctx().local_mut().clear();
        evm.frame_stack().clear();

        // The gas refund is already folded into `gas_used` (via `net_used` in
        // `settle_fees`), so the `refunded` counter is left 0; the per-phase
        // receipt breakdown is deferred (see the module-level "Scope").
        let result_gas = ResultGas::new_with_state_gas(gas_used, 0, 0, 0);
        if calls.reverted {
            // The transaction is still included (nonce consumed, fee paid). Logs
            // from phases that committed before the reverting phase survive.
            Ok(ExecutionResult::Revert { gas: result_gas, logs, output: calls.output })
        } else {
            Ok(ExecutionResult::Success {
                reason: SuccessReason::Return,
                gas: result_gas,
                logs,
                output: Output::Call(calls.output),
            })
        }
    }

    /// Runs the storage-backed pre-call pipeline (authorize, nonce, intrinsic
    /// gas, fee-cap check, account-change apply, auto-delegation) over a gas-free
    /// journal view and publishes the transaction context, returning the resolved
    /// [`Eip8130Outcome`]. Storage writes land on the journal directly; the
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
            let sender_actor = authorized.actors.sender.resolved;
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
            //    in `prepay`; channel and expiring nonces are journal storage.
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
            let Some(execution_gas_available) = intrinsic.execution_gas_available(gas_limit) else {
                return Err(BaseTransactionError::eip8130(
                    "EIP-8130 sender-intrinsic gas exceeds the gas limit",
                ));
            };

            // 7. Fee caps and payer balance.
            FeeCheck::validate_fees(max_fee, max_priority, base_fee)
                .map_err(BaseTransactionError::eip8130)?;
            let payer_balance = sctx
                .with_account_info(payer, |info| Ok(info.balance))
                .map_err(BaseTransactionError::eip8130)?;
            FeeCheck::validate_balance(payer_balance, gas_limit, intrinsic.payer_auth, max_fee)
                .map_err(BaseTransactionError::eip8130)?;

            // 8. Publish the transaction context (sender / payer / actor id) so it
            //    is readable by the `TxContext` precompile during `calls`.
            TxContextStorage::new(sctx)
                .set_context(sender, payer, sender_actor.actor_id)
                .map_err(BaseTransactionError::eip8130)?;

            Ok(Eip8130Outcome {
                sender,
                payer,
                sender_actor_id: sender_actor.actor_id,
                policy_type: sender_actor.policy_type,
                policy_target: sender_actor.policy_target,
                gas_limit,
                sender_intrinsic: intrinsic.sender_intrinsic(),
                payer_auth: intrinsic.payer_auth,
                execution_gas_available,
                effective: FeeCheck::effective_gas_price(max_fee, max_priority, base_fee),
                base_fee,
                bump_protocol_nonce,
            })
        })
    }

    /// Pre-charges the payer the worst-case fee and bumps the sender's protocol
    /// nonce, returning the amount debited. The full `gas_limit` (plus payer
    /// authentication) is reserved at the effective price — alongside the L1 and
    /// worst-case operator fee — so the `calls` cannot spend gas money; the
    /// surplus is refunded in [`Self::settle_fees`]. Also overwrites the
    /// placeholder `TxEnv` caller with the resolved sender so `tx.origin`
    /// (the `ORIGIN` opcode) reads correctly during `calls`.
    fn prepay<DB>(
        ctx: &mut BaseContext<DB>,
        outcome: &Eip8130Outcome,
        encoded: &[u8],
        spec: BaseSpecId,
    ) -> Result<U256, EVMError<DB::Error, BaseTransactionError>>
    where
        DB: AlloyDatabase,
        BaseContext<DB>:
            BaseContextTr + ContextTr<Db = DB, Tx = BaseTransaction<TxEnv>, Block = BlockEnv>,
    {
        ctx.tx.base.caller = outcome.sender;

        if outcome.bump_protocol_nonce {
            let mut sender_acc =
                ctx.journal_mut().load_account_mut(outcome.sender).map_err(EVMError::Database)?;
            sender_acc.bump_nonce();
        }

        // Worst-case chargeable gas: the full sender budget plus payer
        // authentication, both billed at the effective price.
        let max_gas = FeeCheck::max_chargeable_gas(outcome.gas_limit, outcome.payer_auth);
        let gas_charge = U256::from(max_gas)
            .checked_mul(U256::from(outcome.effective))
            .ok_or_else(|| BaseTransactionError::eip8130("EIP-8130 gas pre-charge overflow"))?;
        let l1_cost = ctx.chain_mut().calculate_tx_l1_cost(encoded, spec);
        let operator_cost = ctx.chain().operator_fee_charge(encoded, U256::from(max_gas), spec);
        let prepay = gas_charge
            .checked_add(l1_cost)
            .and_then(|v| v.checked_add(operator_cost))
            .ok_or_else(|| BaseTransactionError::eip8130("EIP-8130 pre-charge overflow"))?;

        let mut payer_acc =
            ctx.journal_mut().load_account_mut(outcome.payer).map_err(EVMError::Database)?;
        let debited = payer_acc.balance().checked_sub(prepay).ok_or_else(|| {
            EVMError::Transaction(BaseTransactionError::eip8130(
                "payer balance is below the worst-case fee",
            ))
        })?;
        payer_acc.set_balance(debited);

        Ok(prepay)
    }

    /// Dispatches the transaction's `calls` as EVM call frames, phase by phase,
    /// from a single gas pool. Each phase runs under a journal checkpoint: a
    /// successful phase commits and its gas refund counts; a reverting phase (or
    /// one blocked by the policy gate) rolls back, is charged for the gas already
    /// consumed without refund, and skips every later phase.
    fn execute_calls<DB, I, P>(
        evm: &mut BaseEvm<DB, I, P>,
        signed: &base_common_consensus::Eip8130Signed,
        outcome: &Eip8130Outcome,
    ) -> Result<CallsResult, EVMError<DB::Error, BaseTransactionError>>
    where
        DB: AlloyDatabase,
        P: PrecompileProvider<BaseContext<DB>, Output = InterpreterResult>,
        BaseContext<DB>: BaseContextTr
            + ContextTr<
                Db = DB,
                Tx = BaseTransaction<TxEnv>,
                Block = BlockEnv,
                Journal: core::fmt::Debug,
            >,
    {
        let mut pool = outcome.execution_gas_available;
        // Signed transaction-level refund counter: refunds are accounted across
        // the whole transaction, not per call. See [`CallsResult::refund`].
        let mut refund: i64 = 0;

        for phase in &signed.tx().calls {
            let checkpoint = evm.ctx_mut().journal_mut().checkpoint();
            let mut phase_refund: i64 = 0;
            let mut phase_reverted = false;
            let mut phase_output = Bytes::new();

            for call in phase {
                // Policy gate: when the authenticating actor is gated, every
                // `call.to` must equal the resolved policy target. A mismatched
                // call is not dispatched and fails the phase deterministically
                // with `ActorPolicyViolation`, charging no call gas for it.
                if outcome.policy_type != 0 && call.to != outcome.policy_target {
                    phase_reverted = true;
                    phase_output =
                        Self::actor_policy_violation_data(outcome.sender_actor_id, call.to);
                    break;
                }

                let frame = Self::run_call(evm, outcome.sender, call.to, call.data.clone(), pool)?;
                let gas = frame.gas();
                // `run_call` caps the frame at `pool`, so a call can never report
                // spending more than the pool held; treat a violation of that EVM
                // invariant as a hard error rather than silently clamping to 0.
                pool = pool.checked_sub(gas.total_gas_spent()).ok_or_else(|| {
                    BaseTransactionError::eip8130(
                        "EIP-8130 call consumed more gas than the phase pool contained",
                    )
                })?;

                let result = frame.interpreter_result().result;
                if result.is_ok() {
                    // Accumulate the call's signed refund (it may be negative)
                    // so offsetting SSTORE refunds across calls cancel exactly,
                    // matching standard transaction-level refund accounting. The
                    // sum is clamped and EIP-3529-capped once in `settle_fees`.
                    phase_refund = phase_refund.saturating_add(gas.refunded());
                } else {
                    phase_reverted = true;
                    phase_output = frame.interpreter_result().output.clone();
                    break;
                }
            }

            if phase_reverted {
                evm.ctx_mut().journal_mut().checkpoint_revert(checkpoint);
                return Ok(CallsResult {
                    call_gas_spent: outcome.execution_gas_available.saturating_sub(pool),
                    refund,
                    reverted: true,
                    output: phase_output,
                });
            }

            // revm's `checkpoint_commit` merges the phase savepoint into its
            // parent without finalizing the journal entries, so a committed phase
            // is still rolled back if the outer checkpoint (taken in `execute`)
            // later reverts — e.g. when a subsequent phase surfaces a database
            // error. Committed phases are only durable once `commit_tx` runs.
            evm.ctx_mut().journal_mut().checkpoint_commit();
            refund = refund.saturating_add(phase_refund);
        }

        Ok(CallsResult {
            call_gas_spent: outcome.execution_gas_available.saturating_sub(pool),
            refund,
            reverted: false,
            output: Bytes::new(),
        })
    }

    /// Dispatches a single protocol call (`from = sender`, `value = 0`) as a
    /// top-level EVM call frame with `gas_limit` and runs it to completion,
    /// returning the [`FrameResult`]. Reuses the Base handler's frame loop; the
    /// inspector is intentionally not driven (see [`BaseEvm::transact_raw`]).
    fn run_call<DB, I, P>(
        evm: &mut BaseEvm<DB, I, P>,
        caller: Address,
        to: Address,
        data: Bytes,
        gas_limit: u64,
    ) -> Result<FrameResult, EVMError<DB::Error, BaseTransactionError>>
    where
        DB: AlloyDatabase,
        P: PrecompileProvider<BaseContext<DB>, Output = InterpreterResult>,
        BaseContext<DB>: BaseContextTr
            + ContextTr<
                Db = DB,
                Tx = BaseTransaction<TxEnv>,
                Block = BlockEnv,
                Journal: core::fmt::Debug,
            >,
    {
        // Resolve the bytecode at `to`, following an EIP-7702 delegation
        // designator to its target (mirrors `create_init_frame`).
        let known_bytecode = {
            let journal = evm.ctx_mut().journal_mut();
            let info = journal.load_account_with_code(to).map_err(EVMError::Database)?.info.clone();
            match info.code.as_ref().and_then(Bytecode::eip7702_address) {
                Some(delegated) => {
                    let target = journal
                        .load_account_with_code(delegated)
                        .map_err(EVMError::Database)?
                        .info
                        .clone();
                    (target.code_hash(), target.code.unwrap_or_default())
                }
                None => (info.code_hash(), info.code.unwrap_or_default()),
            }
        };

        let frame_input = FrameInput::Call(Box::new(CallInputs {
            input: CallInput::Bytes(data),
            return_memory_offset: 0..0,
            gas_limit,
            // Base never enables EIP-8037 (Amsterdam is `ForkCondition::Never`),
            // so the state-gas reservoir is always zero.
            reservoir: 0,
            bytecode_address: to,
            known_bytecode,
            target_address: to,
            caller,
            // `Transfer(ZERO)` is exactly what a zero-value `CALL` opcode lowers
            // to (`Apparent` is reserved for `DELEGATECALL`), so this matches
            // mainnet CALL semantics: `msg.value` reads as 0 and the target is
            // touched. Touching is the correct CALL behaviour and, for an empty
            // target, is a no-op under EIP-161 state-clear (touched-empty is
            // erased at tx end). No new-account gas differs from `Apparent`: the
            // classic 25000 charge lives at the CALL-opcode gas site (which this
            // directly-built frame bypasses) and applies only when value > 0.
            value: CallValue::Transfer(U256::ZERO),
            scheme: CallScheme::Call,
            is_static: false,
            charged_new_account_state_gas: false,
        }));

        // Mirror the mainnet handler's first-frame init: wrap the
        // `LocalContext`'s shared memory buffer rather than allocating a fresh
        // one. The buffer is an `Rc<RefCell<Vec<u8>>>` owned by the context for
        // the EVM's lifetime, so this `Rc::clone` is a refcount bump (not a heap
        // allocation) and every call reuses the same backing allocation, growing
        // it to the high-water mark across calls. When `run_exec_loop` finishes
        // it drops the `SharedMemory` wrapper, which only decrements the refcount
        // — the underlying `Vec` allocation stays owned by the `LocalContext`, so
        // the next call reuses it (there is no "return to pool" step). The per-tx
        // buffer is reclaimed by the `local_mut().clear()` after `commit_tx` in
        // `execute`.
        let ctx = evm.ctx_mut();
        let mut memory =
            SharedMemory::new_with_buffer(Rc::clone(ctx.local().shared_memory_buffer()));
        memory.set_memory_limit(ctx.cfg().memory_limit());
        let frame_init = FrameInit { depth: 0, memory, frame_input };

        let mut handler = BaseHandler::<
            BaseEvm<DB, I, P>,
            EVMError<DB::Error, BaseTransactionError>,
            EthFrame<EthInterpreter>,
        >::new();
        handler.run_exec_loop(evm, frame_init)
    }

    /// Settles the final fee against the pre-charged amount: caps the gas refund,
    /// computes the billable gas, refunds the surplus to the payer, and routes
    /// the base fee, priority tip, L1 cost, and operator fee to their vaults.
    /// Returns the gas used reported in the result.
    #[allow(clippy::too_many_arguments)]
    fn settle_fees<DB>(
        ctx: &mut BaseContext<DB>,
        outcome: &Eip8130Outcome,
        calls: &CallsResult,
        prepay: U256,
        encoded: &[u8],
        spec: BaseSpecId,
        beneficiary: Address,
    ) -> Result<u64, EVMError<DB::Error, BaseTransactionError>>
    where
        DB: AlloyDatabase,
        BaseContext<DB>:
            BaseContextTr + ContextTr<Db = DB, Tx = BaseTransaction<TxEnv>, Block = BlockEnv>,
    {
        // Gas drawn from `gas_limit`: sender-intrinsic plus the gas consumed by
        // calls, less the refund capped at EIP-3529's `gas_used / 5`. The cap
        // denominator (`gross_used`) includes `sender_intrinsic` even though that
        // is a fixed charge with no SSTORE/SELFDESTRUCT refund of its own; this
        // matches the mainnet refund-cap convention, where intrinsic gas counts
        // toward `gas_used` in the `gas_used / 5` ceiling.
        let gross_used = outcome.sender_intrinsic.saturating_add(calls.call_gas_spent);
        let refund = Self::capped_refund(calls.refund, gross_used);
        let net_used = gross_used.saturating_sub(refund);
        // Payer authentication is billed on top of the sender budget.
        let billable_gas = net_used.saturating_add(outcome.payer_auth);

        let fee = U256::from(billable_gas)
            .checked_mul(U256::from(outcome.effective))
            .ok_or_else(|| BaseTransactionError::eip8130("EIP-8130 fee overflow"))?;
        let base_fee_amount = U256::from(billable_gas)
            .checked_mul(U256::from(outcome.base_fee))
            .ok_or_else(|| BaseTransactionError::eip8130("EIP-8130 base-fee amount overflow"))?;
        let priority_amount = fee
            .checked_sub(base_fee_amount)
            .ok_or_else(|| BaseTransactionError::eip8130("EIP-8130 priority amount underflow"))?;
        let l1_cost = ctx.chain_mut().calculate_tx_l1_cost(encoded, spec);
        let operator_cost =
            ctx.chain().operator_fee_charge(encoded, U256::from(billable_gas), spec);

        // Refund the surplus of the worst-case pre-charge. The pre-charge bounds
        // every component above (full `gas_limit`/operator at the effective
        // price), so the subtraction never underflows.
        let total_cost = fee
            .checked_add(l1_cost)
            .and_then(|v| v.checked_add(operator_cost))
            .ok_or_else(|| BaseTransactionError::eip8130("EIP-8130 settled cost overflow"))?;
        let refund_amount = prepay.checked_sub(total_cost).ok_or_else(|| {
            BaseTransactionError::eip8130("settled fee exceeds the worst-case pre-charge")
        })?;

        {
            let mut payer_acc =
                ctx.journal_mut().load_account_mut(outcome.payer).map_err(EVMError::Database)?;
            // Consistent with the checked-arithmetic discipline used elsewhere in
            // this file: `refund_amount <= prepay <= original_balance`, so the sum
            // cannot overflow, but surface a violation rather than minting ETH by
            // clamping to `U256::MAX`.
            let balance = payer_acc.balance().checked_add(refund_amount).ok_or_else(|| {
                BaseTransactionError::eip8130("EIP-8130 payer refund balance overflow")
            })?;
            payer_acc.set_balance(balance);
        }

        for (recipient, amount) in [
            (Predeploys::BASE_FEE_VAULT, base_fee_amount),
            (beneficiary, priority_amount),
            (Predeploys::L1_FEE_VAULT, l1_cost),
            (Predeploys::OPERATOR_FEE_VAULT, operator_cost),
        ] {
            ctx.journal_mut().balance_incr(recipient, amount).map_err(EVMError::Database)?;
        }

        Ok(billable_gas)
    }

    /// Folds the signed transaction-level refund counter into the final applied
    /// refund: clamps a net-negative counter to zero (a net negative grants no
    /// refund, never adds to gas owed), then applies EIP-3529's `gross_used / 5`
    /// ceiling. Accounting refunds signed across the whole transaction — rather
    /// than flooring each call's refund at zero — is what makes offsetting SSTORE
    /// refunds across calls cancel exactly, as under one continuous execution.
    fn capped_refund(signed_refund: i64, gross_used: u64) -> u64 {
        u64::try_from(signed_refund.max(0)).unwrap_or(0).min(gross_used / MAX_REFUND_QUOTIENT)
    }

    /// ABI-encodes the `ActorPolicyViolation(bytes32 actorId, address target)`
    /// protocol revert: the 4-byte selector followed by the two 32-byte words.
    fn actor_policy_violation_data(actor_id: B256, target: Address) -> Bytes {
        // `keccak256(b"ActorPolicyViolation(bytes32,address)")[..4]`, hardcoded to
        // avoid hashing on every policy-gate revert. The
        // `actor_policy_violation_data_is_abi_encoded` test pins this against the
        // canonical signature so it cannot silently drift.
        const SELECTOR: [u8; 4] = [0x1f, 0x1c, 0x0d, 0x27];
        let mut out = Vec::with_capacity(4 + 32 + 32);
        out.extend_from_slice(&SELECTOR);
        out.extend_from_slice(actor_id.as_slice());
        out.extend_from_slice(target.into_word().as_slice());
        Bytes::from(out)
    }
}

#[cfg(test)]
mod tests {
    use alloy_evm::{Evm, FromTxWithEncoded, precompiles::PrecompilesMap};
    use alloy_primitives::{Address, B256, Bytes, U256, address, bytes, keccak256};
    use base_common_consensus::{BaseTxEnvelope, Call, Eip8130Signed, Predeploys, TxEip8130};
    use k256::ecdsa::SigningKey;
    use revm::{
        bytecode::Bytecode,
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

    /// Builds an EVM with `balance` funded to `sender`, optionally deploying
    /// `code` at the given contract addresses.
    fn evm_with_accounts(
        balance: U256,
        sender: Address,
        contracts: &[(Address, Bytes)],
    ) -> BaseEvm<InMemoryDB, NoOpInspector, PrecompilesMap> {
        let mut db = InMemoryDB::default();
        db.insert_account_info(sender, AccountInfo { balance, ..Default::default() });
        for (addr, code) in contracts {
            db.insert_account_info(
                *addr,
                AccountInfo {
                    code_hash: keccak256(code),
                    code: Some(Bytecode::new_raw(code.clone())),
                    ..Default::default()
                },
            );
        }
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

    fn evm_with(
        balance: U256,
        sender: Address,
    ) -> BaseEvm<InMemoryDB, NoOpInspector, PrecompilesMap> {
        evm_with_accounts(balance, sender, &[])
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

    #[test]
    fn single_phase_call_executes_against_contract_code() {
        // Contract that simply STOPs: `0x00`.
        let target = address!("0x00000000000000000000000000000000000000c1");
        let key = signing_key(0x44);
        let sender = eoa_address(&key);

        let mut tx = base_tx();
        tx.calls = vec![vec![Call { to: target, data: Bytes::new() }]];
        let signed = eoa_signed(tx, &key);

        let mut evm = evm_with_accounts(
            U256::from(10u64).pow(U256::from(18u64)),
            sender,
            &[(target, bytes!("00"))],
        );
        let outcome = evm.transact_raw(into_base_tx(&signed)).expect("call tx should execute");
        assert!(outcome.result.is_success(), "expected success, got {:?}", outcome.result);
    }

    #[test]
    fn reverting_call_includes_tx_with_revert_status() {
        // Contract that REVERTs with empty data: PUSH1 0, PUSH1 0, REVERT.
        let target = address!("0x00000000000000000000000000000000000000c2");
        let key = signing_key(0x55);
        let sender = eoa_address(&key);

        let mut tx = base_tx();
        tx.calls = vec![vec![Call { to: target, data: Bytes::new() }]];
        let signed = eoa_signed(tx, &key);

        let initial = U256::from(10u64).pow(U256::from(18u64));
        let mut evm = evm_with_accounts(initial, sender, &[(target, bytes!("60006000fd"))]);
        let outcome = evm.transact_raw(into_base_tx(&signed)).expect("tx should still be included");

        // The phase reverted, but the transaction is included: nonce consumed and
        // the payer charged.
        assert!(matches!(outcome.result, ExecutionResult::Revert { .. }));
        let sender_acc = outcome.state.get(&sender).expect("sender in state");
        assert_eq!(sender_acc.info.nonce, 1);
        assert!(sender_acc.info.balance < initial, "payer should still be charged");
    }

    #[test]
    fn later_phase_skipped_after_earlier_phase_reverts() {
        // Phase 0 reverts; phase 1 must not run. Phase 1 targets a contract that
        // would SSTORE a marker — its absence proves the phase was skipped.
        let reverter = address!("0x00000000000000000000000000000000000000c3");
        let storer = address!("0x00000000000000000000000000000000000000c4");
        let key = signing_key(0x66);
        let sender = eoa_address(&key);

        let mut tx = base_tx();
        tx.calls = vec![
            vec![Call { to: reverter, data: Bytes::new() }],
            vec![Call { to: storer, data: Bytes::new() }],
        ];
        let signed = eoa_signed(tx, &key);

        let mut evm = evm_with_accounts(
            U256::from(10u64).pow(U256::from(18u64)),
            sender,
            &[
                (reverter, bytes!("60006000fd")),
                // PUSH1 1, PUSH1 0, SSTORE, STOP
                (storer, bytes!("600160005500")),
            ],
        );
        let outcome = evm.transact_raw(into_base_tx(&signed)).expect("tx should be included");
        assert!(matches!(outcome.result, ExecutionResult::Revert { .. }));
        // The storer's slot 0 must be unset because phase 1 was skipped.
        let storer_acc = outcome.state.get(&storer);
        let slot0 = storer_acc.and_then(|a| a.storage.get(&U256::ZERO)).map(|s| s.present_value);
        assert!(slot0.is_none() || slot0 == Some(U256::ZERO), "phase 1 should have been skipped");
    }

    /// Canonical Solidity packing of an `ActorConfig` word.
    fn pack_actor(authenticator: Address, scope: u8, expiry: u64, policy_type: u8) -> U256 {
        U256::from_be_slice(authenticator.as_slice())
            | (U256::from(scope) << 160)
            | (U256::from(expiry) << 168)
            | (U256::from(policy_type) << 216)
    }

    /// Signs `tx` for a configured sender as `K1_AUTHENTICATOR || sig`.
    fn configured_signed(tx: TxEip8130, signer: &SigningKey) -> Eip8130Signed {
        let hash = tx.sender_signature_hash();
        let mut auth = Vec::with_capacity(85);
        auth.extend_from_slice(Eip8130Constants::K1_AUTHENTICATOR.as_slice());
        auth.extend_from_slice(&eoa_sig(signer, hash));
        Eip8130Signed::new(tx, Bytes::from(auth), Bytes::new())
    }

    /// Seeds a gated (`policy_type = 1`) `SENDER | PAYER` k1 actor for `account`,
    /// authorized to the `signer` key and gated to `target`, then commits it.
    fn seed_gated_sender(
        evm: &mut BaseEvm<InMemoryDB, NoOpInspector, PrecompilesMap>,
        account: Address,
        signer_addr: Address,
        target: Address,
    ) {
        use base_precompile_storage::Handler as _;
        let actor_id = AccountConfigurationStorage::self_actor_id(signer_addr);
        {
            let ctx = evm.ctx_mut();
            let internals = EvmInternals::from_context(ctx);
            let mut provider = JournalStorageProvider::new(internals, Address::ZERO);
            StorageCtx::enter(&mut provider, |sctx| {
                let mut acc = AccountConfigurationStorage::new(sctx);
                acc.actor_config
                    .at_mut(&actor_id)
                    .at_mut(&account)
                    .write(pack_actor(
                        Eip8130Constants::K1_AUTHENTICATOR,
                        Eip8130Constants::SCOPE_SENDER | Eip8130Constants::SCOPE_PAYER,
                        0,
                        1,
                    ))
                    .unwrap();
                acc.policy_manager.at_mut(&actor_id).at_mut(&account).write(target).unwrap();
            });
        }
        let state = evm.ctx_mut().journal_mut().finalize();
        revm::DatabaseCommit::commit(evm.ctx_mut().journal_mut().db_mut(), state);
    }

    #[test]
    fn policy_gate_blocks_call_to_unauthorized_target() {
        let account = address!("0x00000000000000000000000000000000000000c5");
        let allowed = address!("0x00000000000000000000000000000000000000c6");
        let forbidden = address!("0x00000000000000000000000000000000000000c7");
        let signer = signing_key(0x77);
        let signer_addr = eoa_address(&signer);

        let mut tx = base_tx();
        tx.sender = Some(account);
        tx.calls = vec![vec![Call { to: forbidden, data: Bytes::new() }]];
        let signed = configured_signed(tx, &signer);

        let mut evm = evm_with(U256::from(10u64).pow(U256::from(18u64)), account);
        seed_gated_sender(&mut evm, account, signer_addr, allowed);

        let outcome = evm.transact_raw(into_base_tx(&signed)).expect("tx should be included");
        let ExecutionResult::Revert { output, .. } = &outcome.result else {
            panic!("expected a policy-gate revert, got {:?}", outcome.result);
        };
        let expected_selector = keccak256(b"ActorPolicyViolation(bytes32,address)");
        assert_eq!(&output[..4], &expected_selector[..4], "expected ActorPolicyViolation selector");
        assert_eq!(&output[36..], forbidden.into_word().as_slice(), "target encoded in revert");
    }

    #[test]
    fn policy_gate_allows_call_to_authorized_target() {
        let account = address!("0x00000000000000000000000000000000000000c8");
        let allowed = address!("0x00000000000000000000000000000000000000c9");
        let signer = signing_key(0x88);
        let signer_addr = eoa_address(&signer);

        let mut tx = base_tx();
        tx.sender = Some(account);
        tx.calls = vec![vec![Call { to: allowed, data: Bytes::new() }]];
        let signed = configured_signed(tx, &signer);

        let mut evm = evm_with_accounts(
            U256::from(10u64).pow(U256::from(18u64)),
            account,
            &[(allowed, bytes!("00"))],
        );
        seed_gated_sender(&mut evm, account, signer_addr, allowed);

        let outcome = evm.transact_raw(into_base_tx(&signed)).expect("tx should execute");
        assert!(outcome.result.is_success(), "expected success, got {:?}", outcome.result);
    }

    #[test]
    fn actor_policy_violation_data_is_abi_encoded() {
        let actor_id = B256::repeat_byte(0xab);
        let target = address!("0x00000000000000000000000000000000000000cc");
        let data = Eip8130Executor::actor_policy_violation_data(actor_id, target);
        assert_eq!(data.len(), 68);
        let expected_selector = keccak256(b"ActorPolicyViolation(bytes32,address)");
        assert_eq!(&data[..4], &expected_selector[..4]);
        assert_eq!(&data[4..36], actor_id.as_slice());
        assert_eq!(&data[36..68], target.into_word().as_slice());
    }

    #[test]
    fn capped_refund_uses_signed_transaction_level_accounting() {
        // Two calls touch the same slot: call A clears it (+4800 clear refund),
        // call B re-dirties it back to the original (−4800 for un-clearing,
        // +2900 for the reset-to-original = −1900 net). Transaction-level
        // accounting sums the per-call refunds *signed*: 4800 + (−1900) = 2900,
        // so the clear refund is cancelled down to the reset refund. A per-call
        // floor-at-zero would instead keep 4800 + max(0, −1900) = 4800 and
        // over-refund the payer.
        let signed_sum = 4800_i64 + (-1900_i64);
        assert_eq!(signed_sum, 2900);

        // Large `gross_used` so the EIP-3529 cap does not bind.
        assert_eq!(Eip8130Executor::capped_refund(signed_sum, 1_000_000), 2900);
        assert_eq!(
            Eip8130Executor::capped_refund(4800, 1_000_000),
            4800,
            "a per-call floor would have yielded this larger, incorrect refund",
        );

        // A net-negative counter grants no refund (never adds to gas owed).
        assert_eq!(Eip8130Executor::capped_refund(-1900, 1_000_000), 0);

        // EIP-3529's `gross_used / 5` ceiling still binds the clamped refund.
        assert_eq!(Eip8130Executor::capped_refund(10_000, 20_000), 4_000);
    }

    /// Runs a two-call EIP-8130 transaction over a contract that stores
    /// `calldata[0..32]` into slot 0 (pre-seeded to `1`). Call 1 always clears
    /// the slot (`store 0`); call 2 stores `second_store`. Returns the
    /// transaction's reported `gas_used` plus the slot's final value. The two
    /// calls always carry 32-byte calldata so intrinsic gas and call gas are
    /// identical regardless of the stored value — only the SSTORE *refund*
    /// differs between a restore (`1`) and a no-op re-clear (`0`).
    fn run_cross_call_refund_tx(key_byte: u8, second_store: u8) -> (u64, Option<U256>) {
        // PUSH1 0x00  CALLDATALOAD  PUSH1 0x00  SSTORE  STOP
        let store_calldata = bytes!("60003560005500");
        let target = address!("0x00000000000000000000000000000000000000d1");
        let key = signing_key(key_byte);
        let sender = eoa_address(&key);

        let clear_data = vec![0u8; 32]; // store 0 -> clears slot 0
        let mut second_data = vec![0u8; 32];
        second_data[31] = second_store; // store `second_store`

        let mut tx = base_tx();
        tx.calls = vec![vec![
            Call { to: target, data: Bytes::from(clear_data) },
            Call { to: target, data: Bytes::from(second_data) },
        ]];
        let signed = eoa_signed(tx, &key);

        // Pre-seed slot 0 to a non-zero value so its transaction-start original
        // value is non-zero (a prerequisite for the clear/un-clear refund).
        let initial = U256::from(10u64).pow(U256::from(18u64));
        let mut db = InMemoryDB::default();
        db.insert_account_info(sender, AccountInfo { balance: initial, ..Default::default() });
        db.insert_account_info(
            target,
            AccountInfo {
                code_hash: keccak256(&store_calldata),
                code: Some(Bytecode::new_raw(store_calldata)),
                ..Default::default()
            },
        );
        db.insert_account_storage(target, U256::ZERO, U256::from(1u64)).expect("seed slot 0");
        let mut evm = Context::base()
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
            .build_with_inspector(NoOpInspector);

        let outcome =
            evm.transact_raw(into_base_tx(&signed)).expect("cross-call refund tx should execute");
        assert!(outcome.result.is_success(), "expected success, got {:?}", outcome.result);
        let slot0 = outcome
            .state
            .get(&target)
            .and_then(|a| a.storage.get(&U256::ZERO))
            .map(|s| s.present_value);
        (outcome.result.gas().tx_gas_used(), slot0)
    }

    #[test]
    fn cross_call_offsetting_refunds_cancel() {
        // Both transactions: call 1 clears slot 0 (+4800 clear refund). They
        // differ only in call 2 and so only in the SSTORE refund:
        //   restore  (store 1): re-dirties the slot to its original -> the clear
        //            refund is cancelled (net refund is the smaller reset refund).
        //   re-clear (store 0): slot is already 0, a no-op write with no refund
        //            change -> the +4800 clear refund stands.
        // The SSTORE *gas cost* is identical (both warm, 100), as is intrinsic
        // gas (identical 32-byte calldata and call count), so any difference in
        // `gas_used` is purely the refund. Under correct transaction-level
        // accounting the restore refund is strictly smaller, so the restore tx
        // reports strictly MORE gas_used. The old per-call floor-at-zero would
        // discard the restore's negative delta, making both refunds 4800 and the
        // two gas_used values equal — which this asserts against.
        let (restore_gas, restore_slot) = run_cross_call_refund_tx(0x99, 1);
        let (reclear_gas, reclear_slot) = run_cross_call_refund_tx(0x9a, 0);

        assert_eq!(restore_slot, Some(U256::from(1u64)), "restore should leave the original value");
        assert_eq!(reclear_slot, Some(U256::ZERO), "re-clear should leave the slot cleared");

        assert!(
            restore_gas > reclear_gas,
            "transaction-level accounting must cancel the restore's clear refund, charging more \
             gas than the re-clear case (restore_gas={restore_gas}, reclear_gas={reclear_gas}); \
             equal values indicate per-call refund clamping has regressed",
        );
    }
}
