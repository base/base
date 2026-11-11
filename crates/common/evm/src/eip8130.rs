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
//! The pre-call pipeline, the phased call dispatch, and the settlement arithmetic
//! are the engine-neutral [`Eip8130Pipeline`] and [`Eip8130Calls`] shared with the
//! EVM2 engine; this module supplies the revm journal, frames, and fee routing.
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
//! See [`Eip8130Calls::execute`] for the phase semantics. Each call is dispatched
//! from `sender` to `call.to` with `msg.value == call.value` and
//! `tx.origin == sender`. Each call frame is given the whole remaining pool; as for
//! any EVM frame, a call that halts exceptionally (anything but `REVERT`) consumes
//! all of it.
//!
//! Dispatching a call draws EIP-2929 account-access gas from the pool as a
//! `CALL` would: the cold account access cost for a cold `call.to` or the warm
//! access cost for a warm one, and the same again for the target of an EIP-7702
//! delegation at `call.to`. The precompiles, the coinbase, the
//! sender, the payer, and every account the pre-call pipeline touched start
//! warm.
//!
//! # Scope
//!
//! Protocol-injected account-change logs (`ActorAuthorized`, `ActorRevoked`,
//! `AccountCreated`) are written to the journal during the
//! pre-call apply step and surface in the transaction receipt ahead of any
//! `calls` logs. The number of committed phases (`committedPhases`) is reported
//! on the EIP-8130 receipt; the overall transaction status (all-phases-succeeded vs
//! reverted) is reported through the returned [`ExecutionResult`] variant
//! ([`ExecutionResult::Success`] vs [`ExecutionResult::Revert`]).
//!
//! [transaction context]: base_common_precompiles::TxContextStorage
//! [`BaseEvm::transact_raw`]: crate::BaseEvm

use alloc::{boxed::Box, rc::Rc};

use alloy_evm::{Database as AlloyDatabase, EvmInternals};
use alloy_primitives::{Address, Bytes, U256};
use base_common_consensus::{Call, Eip8130Signed, Predeploys};
use base_execution_eip8130::{
    Eip8130BlockContext, Eip8130CallFrame, Eip8130CallHost, Eip8130Calls, Eip8130CallsResult,
    Eip8130Error, Eip8130FeeSplit, Eip8130Outcome, Eip8130Pipeline,
};
use base_precompile_storage::{JournalStorageProvider, StorageCtx};
use revm::{
    Inspector,
    context::{BlockEnv, LocalContextTr, TxEnv, journaled_state::account::JournaledAccountTr},
    context_interface::{
        Block, Cfg, ContextTr, JournalTr,
        context::take_error,
        result::{EVMError, ExecutionResult, InvalidTransaction, Output, ResultGas, SuccessReason},
    },
    handler::{EthFrame, EvmTr, FrameResult, Handler, PrecompileProvider},
    inspector::{InspectorEvmTr, InspectorHandler, JournalExt},
    interpreter::{
        CallInput, CallInputs, CallOutcome, CallScheme, CallValue, FrameInput, Gas,
        InstructionResult, InterpreterResult, SharedMemory,
        gas::{COLD_ACCOUNT_ACCESS_COST, WARM_STORAGE_READ_COST},
        interpreter::EthInterpreter,
        interpreter_action::FrameInit,
    },
    primitives::{KECCAK_EMPTY, hardfork::SpecId},
    state::Bytecode,
};

use crate::{
    BaseContext, BaseContextTr, BaseEvm, BaseFeature, BaseHaltReason, BaseSpecId, BaseTime,
    BaseTransaction, BaseTransactionError, BaseTxTr, Eip8130CommittedPhases, Eip8130ExecutionMode,
    L1BlockInfo, MeteredGas, handler::BaseHandler,
};

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
    ///
    /// Honors `cfg.disable_fee_charge` exactly like the standard handler does for
    /// other transaction types. Signature verification, nonce validation and the
    /// fee-cap sanity check still run and the sender's protocol nonce is still
    /// bumped, but the payer's balance is not checked against the worst-case fee
    /// (revm's `calculate_caller_fee` returns before `ensure_enough_balance` under
    /// the flag), the payer is neither pre-charged nor settled (no fee reaches the
    /// payer, the vaults, or the beneficiary), and the result reports the
    /// **metered** gas net of the EIP-3529-capped refund (the billable gas without
    /// gas-limit billing) rather than the gas-limit-billing reservation.
    pub fn execute<DB, I, P>(
        evm: &mut BaseEvm<DB, I, P>,
    ) -> Result<ExecutionResult<BaseHaltReason>, EVMError<DB::Error, BaseTransactionError>>
    where
        DB: AlloyDatabase,
        I: Inspector<BaseContext<DB>>,
        P: PrecompileProvider<BaseContext<DB>, Output = InterpreterResult>,
        BaseContext<DB>: BaseContextTr
            + ContextTr<
                Db = DB,
                Tx = BaseTransaction<TxEnv>,
                Block = BlockEnv,
                Journal: core::fmt::Debug + JournalExt,
            >,
    {
        // Discard any committed phase count a previous transaction may have leaked
        // into the thread-local slot (e.g. via a panic caught between its `set` and
        // the `transact` caller's `take`), so this transaction's result can only
        // ever carry its own count. See [`Eip8130CommittedPhases`] panic safety.
        Eip8130CommittedPhases::clear();
        MeteredGas::clear();

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

        // Unlike ordinary typed transactions, the 8130 path bypasses revm's
        // `validate_against_state_and_deduct_caller` (see the L1BlockInfo refresh
        // below), so it does not inherit that path's chain-id check.
        Eip8130Pipeline::check_chain_id(signed.tx(), chain_id).map_err(Self::tx_error)?;

        let spec = ctx.cfg().spec();
        // Consensus-critical: the EIP-8130 transaction type exists only from
        // Everest. Every ingress path gates it, but a block delivered by the
        // Engine API or P2P reaches execution directly.
        if !spec.is_feature_enabled(BaseFeature::Eip8130) {
            return Err(Self::tx_error(Eip8130Pipeline::not_active_error()).into());
        }
        let now =
            Eip8130Pipeline::block_timestamp(ctx.block().timestamp()).map_err(Self::tx_error)?;
        // Everest implies Denim, so `tx[1]` has already written this block's
        // millisecond part to `BaseTime`. Read through the database so the slot
        // is not warmed in the transaction's journal.
        let timestamp_millis_part =
            BaseTime::fetch_timestamp_millis_part(ctx.journal_mut().db_mut())
                .map_err(EVMError::Database)?;
        let now_ms = BaseTime { timestamp_millis_part }.timestamp_ms(now);
        let base_fee: u128 = u128::from(ctx.block().basefee());
        let block_number = ctx.block().number();
        // Reuse the original wire bytes captured during `from_encoded_tx` instead
        // of re-encoding: this avoids an allocation and the assumption that
        // re-encoding is byte-identical, which matters because the L1 data fee
        // is priced from these bytes.
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

        let block = Eip8130BlockContext {
            chain_id,
            now,
            now_ms,
            base_fee,
            tx_gas_limit_cap: ctx.cfg().tx_gas_limit_cap(),
            keystore: spec.is_feature_enabled(BaseFeature::Keystore),
            fee_charge: !ctx.cfg().is_fee_charge_disabled(),
        };
        let outcome = match Self::authorize_and_apply(ctx, &signed, &block) {
            Ok(outcome) => outcome,
            Err(err) => {
                Self::discard_transaction_state(evm);
                return Err(err);
            }
        };

        // Consume the sender's nonce and publish the sender as `tx.origin`, then
        // pre-charge the payer the worst-case fee (so `calls` cannot spend the gas
        // reservation) unless fee charging is disabled, as it is for `eth_call`,
        // `eth_estimateGas` and unvalidated `eth_simulateV1`.
        let fee_charge_disabled = ctx.cfg().is_fee_charge_disabled();
        if let Err(err) = Self::consume_sender_nonce(ctx, &outcome) {
            Self::discard_transaction_state(evm);
            return Err(err);
        }
        let prepay = if fee_charge_disabled {
            U256::ZERO
        } else {
            match Self::prepay(ctx, &outcome, &encoded, spec) {
                Ok(prepay) => prepay,
                Err(err) => {
                    Self::discard_transaction_state(evm);
                    return Err(err);
                }
            }
        };

        // Mirror the mainnet handler's `load_accounts` pre-execution step, which
        // the 8130 path bypasses: set the journal's EVM spec id and warm the
        // precompiles and (EIP-3651) the coinbase before dispatching calls, so a
        // call is charged identically to one in a normal transaction on the same
        // chain.
        if let Err(err) = Self::warm_pre_call_accounts(evm, &outcome) {
            Self::discard_transaction_state(evm);
            return Err(err);
        }

        let inspection =
            Self::start_inspection(evm, outcome.sender, encoded.clone(), outcome.gas_limit);
        let calls =
            match Self::execute_calls(evm, &signed, &outcome, outcome.execution_gas_available) {
                Ok(calls) => calls,
                Err(err) => {
                    Self::end_inspection(
                        evm,
                        inspection,
                        true,
                        Bytes::new(),
                        outcome.gas_limit,
                        outcome.gas_limit,
                    );
                    Self::discard_transaction_state(evm);
                    return Err(err);
                }
            };

        // With fee charging disabled nothing was pre-charged, so there is nothing
        // to settle: report the metered gas, as `eth_call` does for every other
        // transaction type, instead of the reservation that would have been billed.
        let gas_used = if fee_charge_disabled {
            calls.billable_gas(&outcome, false)
        } else {
            match Self::settle_fees(evm.ctx_mut(), &outcome, &calls, prepay, &encoded) {
                Ok(gas_used) => gas_used,
                Err(err) => {
                    Self::end_inspection(
                        evm,
                        inspection,
                        calls.reverted,
                        calls.output.clone(),
                        outcome.gas_limit,
                        outcome.gas_limit,
                    );
                    Self::discard_transaction_state(evm);
                    return Err(err);
                }
            }
        };

        Self::end_inspection(
            evm,
            inspection,
            calls.reverted,
            calls.output.clone(),
            outcome.gas_limit,
            gas_used,
        );
        let ctx = evm.ctx_mut();

        let logs = ctx.journal_mut().take_logs();

        ctx.journal_mut().commit_tx();
        ctx.chain_mut().clear_tx_l1_cost();

        // Parity with the mainnet handler's post-commit cleanup: reclaim the
        // `LocalContext` shared-memory buffer and drain the frame stack so no
        // stale state leaks into the next transaction when the `BaseEvm` is
        // reused across a block.
        evm.ctx().local_mut().clear();
        evm.frame_stack().clear();

        // Hand the committed phase count to the caller of `Evm::transact`, which
        // takes it on this thread as soon as the call returns and carries it with
        // the execution result to the receipt builder (see
        // [`Eip8130CommittedPhases`]). Published as the last step before returning
        // — after the transaction is fully settled and committed — so neither a
        // `settle_fees` error nor a panic in the journal teardown above can leave a
        // stale count in the slot for the next transaction.
        Eip8130CommittedPhases::set(calls.committed_phases);
        // Likewise the metered gas, for the builder's transaction events: what this
        // transaction would have been billed without gas-limit billing, and the
        // payer-authentication gas the reserved budget adds on top of `gas_limit`.
        MeteredGas {
            gas_used: calls.billable_gas(&outcome, false),
            payer_auth_gas_reserved: Some(outcome.payer_auth_cost),
        }
        .set();

        // `gas_used` is already final: the billed budget under gas-limit billing (no refund at
        // all), otherwise metered gas net of the EIP-3529-capped refund (see
        // `Eip8130CallsResult::billable_gas`). The `refunded` counter is therefore left 0.
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

    /// Read-only gas estimation for an EIP-8130 transaction — the
    /// `eth_estimateGas` / `eth_call` path. Runs the same account-change apply,
    /// intrinsic-gas, and phased-`calls` pipeline as
    /// [`Self::execute`] to measure gas, then reverts every journal write so no
    /// state is committed.
    ///
    /// Unlike [`Self::execute`] it performs **no signature verification** and
    /// **no fee settlement**: like `eth_call`/`eth_estimateGas` for every other
    /// transaction type, estimation simulates from the request's `from` without a
    /// signature. The sender actor and its policy are resolved from committed
    /// account state (see [`Eip8130Pipeline::simulate_resolve`]). This entrypoint is
    /// reachable only from the read-only RPC simulation path; block execution and
    /// txpool admission always go through [`Self::execute`] with full verification.
    ///
    /// In [`Eip8130ExecutionMode::Call`] the calls run once at the
    /// transaction's gas limit and the reported gas is that run's metered gas.
    /// In [`Eip8130ExecutionMode::Simulate`] the reported gas is the searched
    /// gas limit at which the calls succeed.
    ///
    /// Under gas-limit billing the `Simulate` figure is the sender's budget only:
    /// it leaves out payer authentication, which settlement adds on top as
    /// `payer_auth_cost`, so it can be used directly as `gas_limit`. The `Call`-mode
    /// figure (and [`Self::simulate_single_run`]) still includes
    /// `payer_auth_cost`; no RPC exposes it as a gas limit today.
    pub fn simulate<DB, I, P>(
        evm: &mut BaseEvm<DB, I, P>,
    ) -> Result<ExecutionResult<BaseHaltReason>, EVMError<DB::Error, BaseTransactionError>>
    where
        DB: AlloyDatabase,
        I: Inspector<BaseContext<DB>>,
        P: PrecompileProvider<BaseContext<DB>, Output = InterpreterResult>,
        BaseContext<DB>: BaseContextTr
            + ContextTr<
                Db = DB,
                Tx = BaseTransaction<TxEnv>,
                Block = BlockEnv,
                Journal: core::fmt::Debug + JournalExt,
            >,
    {
        // Simulation settles nothing and publishes no metered gas, but it still runs on
        // a thread that may later `take()`: clear the slot so an unread value from an
        // earlier transaction cannot be attributed to a later one.
        MeteredGas::clear();

        // Clone the envelope + optional acting-actor hint before taking a mutable
        // borrow of `ctx` (same pattern as `execute`).
        let (signed, acting_actor_hint, mode) = evm
            .ctx()
            .tx()
            .eip8130_parts()
            .map(|parts| (parts.signed.clone(), parts.simulation_sender_actor_id, parts.mode))
            .ok_or_else(|| {
                BaseTransactionError::eip8130("transaction is not an EIP-8130 transaction")
            })?;

        let ctx = evm.ctx_mut();
        let spec = ctx.cfg().spec();
        if !spec.is_feature_enabled(BaseFeature::Eip8130) {
            return Err(Self::tx_error(Eip8130Pipeline::not_active_error()).into());
        }
        let from = ctx.tx().base.caller;
        // Estimation applies account changes exactly as consensus would, including
        // the per-change JIT expiry skip, which depends on the block timestamp (in
        // Unix seconds). Using the estimation block's timestamp can only over-price
        // a grant that expires before inclusion — it never under-estimates.
        let now =
            Eip8130Pipeline::block_timestamp(ctx.block().timestamp()).map_err(Self::tx_error)?;
        let block = Eip8130BlockContext {
            chain_id: ctx.cfg().chain_id(),
            now,
            // Simulation never validates the window or the nonce-free replay ring.
            now_ms: 0,
            base_fee: u128::from(ctx.block().basefee()),
            tx_gas_limit_cap: ctx.cfg().tx_gas_limit_cap(),
            keystore: spec.is_feature_enabled(BaseFeature::Keystore),
            // Simulation never checks the payer's balance.
            fee_charge: false,
        };
        let encoded =
            ctx.tx().enveloped_tx().cloned().ok_or_else(|| {
                BaseTransactionError::eip8130("missing enveloped transaction bytes")
            })?;

        let outcome = match Self::simulate_resolve(ctx, &signed, from, acting_actor_hint, &block) {
            Ok(outcome) => outcome,
            Err(err) => {
                Self::discard_transaction_state(evm);
                return Err(err);
            }
        };

        // Dispatch `calls` from the resolved sender so `tx.origin` reads correctly
        // and bump its protocol nonce, as the execution path does.
        if let Err(err) = Self::consume_sender_nonce(evm.ctx_mut(), &outcome) {
            Self::discard_transaction_state(evm);
            return Err(err);
        }

        // Warm precompiles/coinbase and set the journal spec id exactly as
        // [`Self::execute`] does, so the simulated `calls` are charged the same
        // warm-access gas a real execution would — keeping the estimate aligned.
        if let Err(err) = Self::warm_pre_call_accounts(evm, &outcome) {
            Self::discard_transaction_state(evm);
            return Err(err);
        }
        let inspection = Self::start_inspection(evm, outcome.sender, encoded, outcome.gas_limit);

        let ceiling_pool = outcome.execution_gas_available;
        if mode == Eip8130ExecutionMode::Call {
            return Self::simulate_single_run(evm, &signed, &outcome, inspection, ceiling_pool);
        }

        // The estimate must return a gas *limit* that guarantees execution
        // succeeds, not the net charge. Refunds never rejoin the call pool, and
        // EIP-150's 63/64 forwarding can starve a deep callee at a tighter limit,
        // so search for the minimum call pool at which the calls still succeed —
        // re-dispatching them at candidate pools over fresh journal checkpoints
        // (each reverted), exactly as standard `eth_estimateGas` binary-searches
        // the gas limit. The first run at the request's full pool measures the
        // baseline call spend and decides whether the transaction can succeed at
        // all.
        //
        // Probes are implementation details of gas estimation, not user-visible
        // executions. Suppress inspection while probing so `debug_traceCall`
        // records only the final canonical run rather than every bisection
        // candidate.
        let inspect = core::mem::replace(&mut evm.inspect, false);
        let ceiling = match Self::probe_calls(evm, &signed, &outcome, ceiling_pool) {
            Ok(ceiling) => ceiling,
            Err(err) => {
                evm.inspect = inspect;
                Self::end_inspection(
                    evm,
                    inspection,
                    true,
                    Bytes::new(),
                    outcome.gas_limit,
                    outcome.gas_limit,
                );
                Self::discard_transaction_state(evm);
                return Err(err);
            }
        };

        // A revert/halt even at the full pool is a genuine failure (not a gas
        // shortfall the search could fix): surface it like standard
        // `eth_estimateGas`. Re-run once un-reverted to capture the revert output
        // and any logs the committed phases emitted before the failing phase.
        if ceiling.reverted {
            evm.inspect = inspect;
            return Self::simulate_single_run(evm, &signed, &outcome, inspection, ceiling_pool);
        }

        let feasible_pool =
            match Eip8130Calls::search_estimate_pool(ceiling.call_gas_spent, ceiling_pool, |pool| {
                Ok(Self::probe_calls(evm, &signed, &outcome, pool)?.reverted)
            }) {
                Ok(pool) => pool,
                Err(err) => {
                    evm.inspect = inspect;
                    Self::end_inspection(
                        evm,
                        inspection,
                        true,
                        Bytes::new(),
                        outcome.gas_limit,
                        outcome.gas_limit,
                    );
                    Self::discard_transaction_state(evm);
                    return Err(err);
                }
            };

        // Final canonical run at the chosen pool (un-reverted) to capture the logs
        // and output at the returned gas limit, then discard all simulated state.
        evm.inspect = inspect;
        let final_calls = match Self::execute_calls(evm, &signed, &outcome, feasible_pool) {
            Ok(final_calls) => final_calls,
            Err(err) => {
                Self::end_inspection(
                    evm,
                    inspection,
                    true,
                    Bytes::new(),
                    outcome.gas_limit,
                    outcome.gas_limit,
                );
                Self::discard_transaction_state(evm);
                return Err(err);
            }
        };
        let logs = evm.ctx_mut().journal_mut().take_logs();

        // gas_limit = max(intrinsic + feasible_pool, sender_floor) + payer_auth_cost.
        // The on-chain call pool at this limit is `gas_limit - intrinsic` (payer
        // authentication is billed on top of the limit, not drawn from the pool),
        // so it is at least `feasible_pool` — the verified-feasible amount — and
        // the limit also covers the net charge (which never exceeds it). The
        // EIP-7623 floor raises the returned limit for a data-heavy transaction so
        // it is never rejected at admission (`gas_limit >= sender_floor`).
        //
        // Under gas-limit billing the declared limit is billed as-is and payer
        // authentication is billed on top of it, so folding `payer_auth_cost`
        // into the returned limit would charge it twice. The estimate is then the
        // sender budget alone: a transaction submitted at `gas_limit = estimate`
        // settles exactly `estimate + payer_auth_cost`, the reserved budget.
        let sender_budget =
            outcome.floored_sender_gas(outcome.sender_intrinsic.saturating_add(feasible_pool));
        let estimate_gas =
            if evm.ctx().cfg().spec().is_feature_enabled(BaseFeature::GasLimitBilling) {
                sender_budget
            } else {
                sender_budget.saturating_add(outcome.payer_auth_cost)
            };
        Self::end_inspection(
            evm,
            inspection,
            final_calls.reverted,
            final_calls.output.clone(),
            outcome.gas_limit,
            estimate_gas,
        );
        Self::discard_transaction_state(evm);
        let result_gas = ResultGas::new_with_state_gas(estimate_gas, 0, 0, 0);
        if final_calls.reverted {
            Ok(ExecutionResult::Revert { gas: result_gas, logs, output: final_calls.output })
        } else {
            Ok(ExecutionResult::Success {
                reason: SuccessReason::Return,
                gas: result_gas,
                logs,
                output: Output::Call(final_calls.output),
            })
        }
    }

    /// Runs the phased `calls` once at `pool` (un-reverted, so the output and
    /// logs are captured), closes the trace root, and discards all simulated
    /// state. Reports the metered gas of that run, with the EIP-7623 sender
    /// floor applied, as a success or a revert.
    fn simulate_single_run<DB, I, P>(
        evm: &mut BaseEvm<DB, I, P>,
        signed: &Eip8130Signed,
        outcome: &Eip8130Outcome,
        inspection: Option<CallInputs>,
        pool: u64,
    ) -> Result<ExecutionResult<BaseHaltReason>, EVMError<DB::Error, BaseTransactionError>>
    where
        DB: AlloyDatabase,
        I: Inspector<BaseContext<DB>>,
        P: PrecompileProvider<BaseContext<DB>, Output = InterpreterResult>,
        BaseContext<DB>: BaseContextTr
            + ContextTr<
                Db = DB,
                Tx = BaseTransaction<TxEnv>,
                Block = BlockEnv,
                Journal: core::fmt::Debug + JournalExt,
            >,
    {
        let calls = match Self::execute_calls(evm, signed, outcome, pool) {
            Ok(calls) => calls,
            Err(err) => {
                Self::end_inspection(
                    evm,
                    inspection,
                    true,
                    Bytes::new(),
                    outcome.gas_limit,
                    outcome.gas_limit,
                );
                Self::discard_transaction_state(evm);
                return Err(err);
            }
        };
        let logs = evm.ctx_mut().journal_mut().take_logs();
        let gross = calls.gross_gas(outcome);
        Self::end_inspection(
            evm,
            inspection,
            calls.reverted,
            calls.output.clone(),
            outcome.gas_limit,
            gross,
        );
        Self::discard_transaction_state(evm);
        let gas = ResultGas::new_with_state_gas(gross, 0, 0, 0);
        if calls.reverted {
            Ok(ExecutionResult::Revert { gas, logs, output: calls.output })
        } else {
            Ok(ExecutionResult::Success {
                reason: SuccessReason::Return,
                gas,
                logs,
                output: Output::Call(calls.output),
            })
        }
    }

    /// Runs the phased `calls` at `pool` under a nested journal checkpoint and
    /// reverts every write (and log) they made, leaving the journal at the
    /// resolved pre-call state so the next probe starts identically. Used only by
    /// the [`Self::simulate`] gas-limit search.
    fn probe_calls<DB, I, P>(
        evm: &mut BaseEvm<DB, I, P>,
        signed: &Eip8130Signed,
        outcome: &Eip8130Outcome,
        pool: u64,
    ) -> Result<Eip8130CallsResult, EVMError<DB::Error, BaseTransactionError>>
    where
        DB: AlloyDatabase,
        I: Inspector<BaseContext<DB>>,
        P: PrecompileProvider<BaseContext<DB>, Output = InterpreterResult>,
        BaseContext<DB>: BaseContextTr
            + ContextTr<
                Db = DB,
                Tx = BaseTransaction<TxEnv>,
                Block = BlockEnv,
                Journal: core::fmt::Debug + JournalExt,
            >,
    {
        let checkpoint = evm.ctx_mut().journal_mut().checkpoint();
        let calls = Self::execute_calls(evm, signed, outcome, pool)?;
        evm.ctx_mut().journal_mut().checkpoint_revert(checkpoint);
        Ok(calls)
    }

    /// Runs [`Eip8130Pipeline::simulate_resolve`] over a gas-free journal view.
    /// Storage writes land on the journal; the caller's checkpoint reverts them.
    fn simulate_resolve<DB>(
        ctx: &mut BaseContext<DB>,
        signed: &Eip8130Signed,
        sender: Address,
        acting_actor_hint: Option<alloy_primitives::B256>,
        block: &Eip8130BlockContext,
    ) -> Result<Eip8130Outcome, EVMError<DB::Error, BaseTransactionError>>
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
        let internals = EvmInternals::from_context(ctx);
        let mut provider = JournalStorageProvider::new(internals, Address::ZERO);
        let outcome = StorageCtx::enter(&mut provider, |sctx| {
            Eip8130Pipeline::simulate_resolve(sctx, signed, sender, acting_actor_hint, block)
        });
        Self::finish_pre_call(&mut provider, outcome)
    }

    /// Checks the validity window, then runs [`Eip8130Pipeline::authorize_and_apply`]
    /// over a gas-free journal view. Storage writes land on the journal directly;
    /// the caller discards the transaction on error.
    fn authorize_and_apply<DB>(
        ctx: &mut BaseContext<DB>,
        signed: &Eip8130Signed,
        block: &Eip8130BlockContext,
    ) -> Result<Eip8130Outcome, EVMError<DB::Error, BaseTransactionError>>
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
        Eip8130Pipeline::check_validity_window(signed.tx(), block.now_ms)
            .map_err(Self::tx_error)?;
        let internals = EvmInternals::from_context(ctx);
        let mut provider = JournalStorageProvider::new(internals, Address::ZERO);
        let outcome = StorageCtx::enter(&mut provider, |sctx| {
            Eip8130Pipeline::authorize_and_apply(sctx, signed, block)
        });
        Self::finish_pre_call(&mut provider, outcome)
    }

    /// Resolves the outcome of a storage-backed pre-call pipeline. A journal
    /// database failure observed anywhere in the pipeline is fatal, even when the
    /// pipeline stringified it into a transaction error or swallowed it: the
    /// state it read cannot be trusted, and reporting it as an invalid
    /// transaction would wrongly reject the block (or the transaction) instead of
    /// failing the node.
    fn finish_pre_call<E>(
        provider: &mut JournalStorageProvider<'_>,
        outcome: Result<Eip8130Outcome, Eip8130Error>,
    ) -> Result<Eip8130Outcome, EVMError<E, BaseTransactionError>> {
        if let Some(error) = provider.take_database_error() {
            return Err(EVMError::Custom(error));
        }
        outcome.map_err(|error| EVMError::Transaction(Self::tx_error(error)))
    }

    /// Bumps the sender's protocol nonce (when the transaction uses the protocol
    /// nonce channel) and overwrites the placeholder `TxEnv` caller with the
    /// resolved sender so `tx.origin` (the `ORIGIN` opcode) reads correctly
    /// during `calls`. Runs for every executed transaction, whether or not fees
    /// are charged, and in [`Self::simulate`]: a simulation must still observe
    /// the nonce bump and the real origin.
    fn consume_sender_nonce<DB>(
        ctx: &mut BaseContext<DB>,
        outcome: &Eip8130Outcome,
    ) -> Result<(), EVMError<DB::Error, BaseTransactionError>>
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
        Ok(())
    }

    /// Pre-charges the payer the worst-case fee, returning the amount debited.
    /// The full `gas_limit` (plus payer authentication) is reserved at the
    /// effective price — alongside the L1 and worst-case operator fee — so the
    /// `calls` cannot spend gas money. [`Self::settle_fees`] refunds the surplus
    /// over the billed gas; under gas-limit billing the billed gas is the whole
    /// reservation, so the gas surplus is zero. Skipped entirely when fee
    /// charging is disabled.
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
        let max_gas = outcome.max_chargeable_gas();
        let gas_charge = U256::from(max_gas)
            .checked_mul(U256::from(outcome.effective))
            .ok_or_else(|| BaseTransactionError::eip8130("EIP-8130 gas pre-charge overflow"))?;
        // `encoded` is the transaction's enveloped bytes, so any cached FastLZ size applies.
        let fastlz_size = ctx.tx.enveloped_tx_fastlz_size;
        let l1_cost =
            ctx.chain_mut().calculate_tx_l1_cost_with_fastlz_size(encoded, fastlz_size, spec);
        let operator_cost = ctx.chain().operator_fee_charge(encoded, U256::from(max_gas), spec);
        let prepay = gas_charge
            .checked_add(l1_cost)
            .and_then(|v| v.checked_add(operator_cost))
            .ok_or_else(|| BaseTransactionError::eip8130("EIP-8130 pre-charge overflow"))?;

        let mut payer_acc =
            ctx.journal_mut().load_account_mut(outcome.payer).map_err(EVMError::Database)?;
        let balance = *payer_acc.balance();
        let debited = balance
            .checked_sub(prepay)
            .ok_or_else(|| EVMError::Transaction(Self::payer_cannot_pay(balance, prepay)))?;
        payer_acc.set_balance(debited);

        Ok(prepay)
    }

    /// Dispatches the transaction's `calls` as EVM call frames through
    /// [`Eip8130Calls::execute`], from a single gas `pool`.
    fn execute_calls<DB, I, P>(
        evm: &mut BaseEvm<DB, I, P>,
        signed: &Eip8130Signed,
        outcome: &Eip8130Outcome,
        pool: u64,
    ) -> Result<Eip8130CallsResult, EVMError<DB::Error, BaseTransactionError>>
    where
        DB: AlloyDatabase,
        I: Inspector<BaseContext<DB>>,
        P: PrecompileProvider<BaseContext<DB>, Output = InterpreterResult>,
        BaseContext<DB>: BaseContextTr
            + ContextTr<
                Db = DB,
                Tx = BaseTransaction<TxEnv>,
                Block = BlockEnv,
                Journal: core::fmt::Debug + JournalExt,
            >,
    {
        Eip8130Calls::execute(&mut RevmCallHost { evm }, signed, outcome, pool)
    }

    /// Opens the synthetic transaction root used to attach inspected EIP-8130
    /// protocol calls to one connected trace arena.
    fn start_inspection<DB, I, P>(
        evm: &mut BaseEvm<DB, I, P>,
        sender: Address,
        encoded: Bytes,
        gas_limit: u64,
    ) -> Option<CallInputs>
    where
        DB: AlloyDatabase,
        I: Inspector<BaseContext<DB>>,
        P: PrecompileProvider<BaseContext<DB>, Output = InterpreterResult>,
        BaseContext<DB>: BaseContextTr
            + ContextTr<
                Db = DB,
                Tx = BaseTransaction<TxEnv>,
                Block = BlockEnv,
                Journal: core::fmt::Debug + JournalExt,
            >,
    {
        if !evm.inspect {
            return None;
        }
        let mut inputs = CallInputs {
            input: CallInput::Bytes(encoded),
            return_memory_offset: 0..0,
            gas_limit,
            reservoir: 0,
            bytecode_address: sender,
            known_bytecode: (KECCAK_EMPTY, Bytecode::default()),
            target_address: sender,
            caller: sender,
            value: CallValue::Transfer(U256::ZERO),
            scheme: CallScheme::Call,
            is_static: false,
            charged_new_account_state_gas: false,
        };
        let (ctx, inspector) = evm.ctx_inspector();
        let _ = inspector.call(ctx, &mut inputs);
        Some(inputs)
    }

    /// Closes the synthetic EIP-8130 transaction trace root.
    fn end_inspection<DB, I, P>(
        evm: &mut BaseEvm<DB, I, P>,
        inputs: Option<CallInputs>,
        reverted: bool,
        output: Bytes,
        gas_limit: u64,
        gas_used: u64,
    ) where
        DB: AlloyDatabase,
        I: Inspector<BaseContext<DB>>,
        P: PrecompileProvider<BaseContext<DB>, Output = InterpreterResult>,
        BaseContext<DB>: BaseContextTr
            + ContextTr<
                Db = DB,
                Tx = BaseTransaction<TxEnv>,
                Block = BlockEnv,
                Journal: core::fmt::Debug + JournalExt,
            >,
    {
        let Some(inputs) = inputs else { return };
        // A sponsored transaction settles `gas_limit + payer_auth_cost` under gas-limit billing, more
        // than the sender's `gas_limit`; widen the root so it reports what the receipt bills.
        let mut gas = Gas::new(gas_limit.max(gas_used));
        let _ = gas.record_regular_cost(gas_used);
        let mut outcome = CallOutcome::new(
            InterpreterResult {
                result: if reverted {
                    InstructionResult::Revert
                } else {
                    InstructionResult::Return
                },
                output,
                gas,
            },
            0..0,
        );
        let (ctx, inspector) = evm.ctx_inspector();
        inspector.call_end(ctx, &inputs, &mut outcome);
    }

    /// Mirrors the mainnet handler's `load_accounts` pre-execution step (minus
    /// the access-list handling, for which an EIP-8130 transaction has no
    /// analogue): sets the journal's EVM spec id, warms the precompile address
    /// set, and warms the coinbase per EIP-3651 (Shanghai+). The 8130 path runs
    /// `calls` directly without the single-frame handler, so without this the
    /// dispatched calls would execute against the journal's default spec id and
    /// a cold coinbase / precompile set — charging cold-access gas (2600) where a
    /// call in a normal transaction is charged warm (100) and otherwise drifting
    /// from EVM equivalence on the same chain.
    ///
    /// The sender and payer are warm like a normal transaction's caller. The
    /// execution pipeline has already loaded both; loading them here gives the
    /// estimate, which never pre-charges the payer, the same warm set.
    fn warm_pre_call_accounts<DB, I, P>(
        evm: &mut BaseEvm<DB, I, P>,
        outcome: &Eip8130Outcome,
    ) -> Result<(), EVMError<DB::Error, BaseTransactionError>>
    where
        DB: AlloyDatabase,
        P: PrecompileProvider<BaseContext<DB>, Output = InterpreterResult>,
        BaseContext<DB>:
            BaseContextTr + ContextTr<Db = DB, Tx = BaseTransaction<TxEnv>, Block = BlockEnv>,
    {
        let (ctx, precompiles) = evm.ctx_precompiles();

        let gen_spec = ctx.cfg().spec();
        let eth_spec: SpecId = gen_spec.into();
        ctx.journal_mut().set_spec_id(eth_spec);

        // Inject the precompile addresses when the spec changed them or the
        // journal has not been warmed yet, matching `pre_execution::load_accounts`.
        let precompiles_changed = precompiles.set_spec(gen_spec);
        if precompiles_changed || ctx.journal_mut().precompile_addresses().is_empty() {
            ctx.journal_mut().warm_precompiles(precompiles.warm_addresses());
        }

        // EIP-3651: the COINBASE address starts warm from Shanghai onward.
        if eth_spec.is_enabled_in(SpecId::SHANGHAI) {
            let coinbase = ctx.block().beneficiary();
            ctx.journal_mut().warm_coinbase_account(coinbase);
        }

        for account in [outcome.sender, outcome.payer] {
            ctx.journal_mut().load_account(account).map_err(EVMError::Database)?;
        }
        Ok(())
    }

    /// Discards all transaction-scoped EVM state.
    ///
    /// Used both after execution failures and to roll back successful read-only
    /// simulations. Mirrors the mainnet handler's `catch_error` cleanup by
    /// discarding the transaction, clearing the cached L1 cost, and draining the
    /// frame stack and local context. A database error raised inside a nested
    /// subcall surfaces while the parent frame is still on the stack, so draining
    /// both prevents stale frame/local state from leaking into the next
    /// transaction when a `BaseEvm` is reused.
    fn discard_transaction_state<DB, I, P>(evm: &mut BaseEvm<DB, I, P>)
    where
        DB: AlloyDatabase,
        P: PrecompileProvider<BaseContext<DB>, Output = InterpreterResult>,
        BaseContext<DB>:
            BaseContextTr + ContextTr<Db = DB, Tx = BaseTransaction<TxEnv>, Block = BlockEnv>,
    {
        let ctx = evm.ctx_mut();
        ctx.journal_mut().discard_tx();
        ctx.chain_mut().clear_tx_l1_cost();
        evm.ctx().local_mut().clear();
        evm.frame_stack().clear();
    }

    /// Settles the final fee against the pre-charged amount: computes the billable
    /// gas ([`Eip8130CallsResult::billable_gas`]), refunds the surplus to the payer,
    /// and routes the base fee, priority tip, L1 cost, and operator fee to their
    /// vaults. Returns the gas used reported in the result. Only runs when fees are
    /// charged; a fee-charge-disabled execution reports the metered gas instead and
    /// moves no balances.
    fn settle_fees<DB>(
        ctx: &mut BaseContext<DB>,
        outcome: &Eip8130Outcome,
        calls: &Eip8130CallsResult,
        prepay: U256,
        encoded: &[u8],
    ) -> Result<u64, EVMError<DB::Error, BaseTransactionError>>
    where
        DB: AlloyDatabase,
        BaseContext<DB>:
            BaseContextTr + ContextTr<Db = DB, Tx = BaseTransaction<TxEnv>, Block = BlockEnv>,
    {
        let spec = ctx.cfg().spec();
        // Under gas-limit billing the charge is the whole reserved budget.
        let billable_gas =
            calls.billable_gas(outcome, spec.is_feature_enabled(BaseFeature::GasLimitBilling));
        let split = Eip8130FeeSplit::new(outcome, billable_gas).map_err(Self::tx_error)?;
        let fastlz_size = ctx.tx.enveloped_tx_fastlz_size;
        let l1_cost =
            ctx.chain_mut().calculate_tx_l1_cost_with_fastlz_size(encoded, fastlz_size, spec);
        let operator_cost =
            ctx.chain().operator_fee_charge(encoded, U256::from(billable_gas), spec);

        // Refund the surplus of the worst-case pre-charge. The pre-charge bounds
        // every component above (full `gas_limit`/operator at the effective
        // price), so the subtraction never underflows.
        let total_cost = split
            .fee
            .checked_add(l1_cost)
            .and_then(|v| v.checked_add(operator_cost))
            .ok_or_else(|| BaseTransactionError::eip8130("EIP-8130 settled cost overflow"))?;
        let refund_amount = prepay.checked_sub(total_cost).ok_or_else(|| {
            BaseTransactionError::eip8130("settled fee exceeds the worst-case pre-charge")
        })?;

        {
            let mut payer_acc =
                ctx.journal_mut().load_account_mut(outcome.payer).map_err(EVMError::Database)?;
            // `refund_amount <= prepay <= original_balance`, so the sum cannot
            // overflow, but surface a violation rather than minting ETH by
            // clamping to `U256::MAX`.
            let balance = payer_acc.balance().checked_add(refund_amount).ok_or_else(|| {
                BaseTransactionError::eip8130("EIP-8130 payer refund balance overflow")
            })?;
            payer_acc.set_balance(balance);
        }

        let beneficiary = ctx.block().beneficiary();
        for (recipient, amount) in [
            (Predeploys::BASE_FEE_VAULT, split.base_fee_amount),
            (beneficiary, split.priority_amount),
            (Predeploys::L1_FEE_VAULT, l1_cost),
            (Predeploys::OPERATOR_FEE_VAULT, operator_cost),
        ] {
            ctx.journal_mut().balance_incr(recipient, amount).map_err(EVMError::Database)?;
        }

        Ok(billable_gas)
    }

    /// Maps an engine-neutral EIP-8130 rejection onto the revm transaction error.
    fn tx_error(error: Eip8130Error) -> BaseTransactionError {
        match error {
            Eip8130Error::Invalid(reason) => BaseTransactionError::eip8130(reason),
            Eip8130Error::PayerCannotPay { balance, required } => {
                Self::payer_cannot_pay(balance, required)
            }
        }
    }

    /// The rejection for a payer whose balance cannot cover the worst-case fee.
    ///
    /// Reported as revm's `LackOfFundForMaxFee` (not an EIP-8130-specific
    /// error) so the builder skips the transaction like any other one that
    /// cannot pay, rather than aborting the payload: a payer's balance can
    /// change after admission, including earlier in the same block.
    fn payer_cannot_pay(balance: U256, required: U256) -> BaseTransactionError {
        BaseTransactionError::Base(InvalidTransaction::LackOfFundForMaxFee {
            fee: Box::new(required),
            balance: Box::new(balance),
        })
    }
}

/// The revm journal and frame loop as the [`Eip8130CallHost`] of [`Eip8130Calls::execute`].
struct RevmCallHost<'a, DB: AlloyDatabase, I, P> {
    evm: &'a mut BaseEvm<DB, I, P>,
}

impl<DB, I, P> Eip8130CallHost for RevmCallHost<'_, DB, I, P>
where
    DB: AlloyDatabase,
    I: Inspector<BaseContext<DB>>,
    P: PrecompileProvider<BaseContext<DB>, Output = InterpreterResult>,
    BaseContext<DB>: BaseContextTr
        + ContextTr<
            Db = DB,
            Tx = BaseTransaction<TxEnv>,
            Block = BlockEnv,
            Journal: core::fmt::Debug + JournalExt,
        >,
{
    type Checkpoint = revm::context::journaled_state::JournalCheckpoint;
    type Error = EVMError<DB::Error, BaseTransactionError>;

    fn checkpoint(&mut self) -> Self::Checkpoint {
        self.evm.ctx_mut().journal_mut().checkpoint()
    }

    fn revert(&mut self, checkpoint: Self::Checkpoint) {
        self.evm.ctx_mut().journal_mut().checkpoint_revert(checkpoint);
    }

    /// revm's `checkpoint_commit` merges the phase savepoint into its parent
    /// without finalizing the journal entries, so a committed phase is still
    /// rolled back if the transaction is ultimately discarded (e.g. when a
    /// subsequent phase surfaces a database error). Committed phases are only
    /// durable once `commit_tx` runs.
    fn commit(&mut self, _checkpoint: Self::Checkpoint) {
        self.evm.ctx_mut().journal_mut().checkpoint_commit();
    }

    fn root_call_access_gas(
        &mut self,
        to: Address,
        target_prepaid: bool,
    ) -> Result<u64, Self::Error> {
        let access_cost =
            |is_cold: bool| if is_cold { COLD_ACCOUNT_ACCESS_COST } else { WARM_STORAGE_READ_COST };
        let journal = self.evm.ctx_mut().journal_mut();
        let target = journal.load_account_with_code(to).map_err(EVMError::Database)?;
        let mut gas = if target_prepaid { 0 } else { access_cost(target.is_cold) };
        let delegate = target.data.info.code.as_ref().and_then(Bytecode::eip7702_address);
        if let Some(delegate) = delegate {
            let delegate = journal.load_account_with_code(delegate).map_err(EVMError::Database)?;
            gas += access_cost(delegate.is_cold);
        }
        Ok(gas)
    }

    fn creates_account(
        &mut self,
        sender: Address,
        to: Address,
        value: U256,
    ) -> Result<bool, Self::Error> {
        if value.is_zero() || to == sender {
            return Ok(false);
        }
        let journal = self.evm.ctx_mut().journal_mut();
        // `to` was already loaded (and its access charged) by
        // `root_call_access_gas`, and `sender` is warm from the pre-call
        // pipeline, so these loads warm nothing new.
        if !journal.load_account(to).map_err(EVMError::Database)?.data.is_empty() {
            return Ok(false);
        }
        let balance = journal.load_account(sender).map_err(EVMError::Database)?.data.info.balance;
        Ok(balance >= value)
    }

    /// Dispatches the call as a top-level EVM call frame, reusing the Base
    /// handler's frame loop and driving the configured inspector when inspection
    /// is enabled.
    fn run_call(
        &mut self,
        caller: Address,
        call: &Call,
        gas_limit: u64,
    ) -> Result<Eip8130CallFrame, Self::Error> {
        let evm = &mut *self.evm;
        let to = call.to;
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
            input: CallInput::Bytes(call.data.clone()),
            return_memory_offset: 0..0,
            gas_limit,
            // Base never enables EIP-8037 (Amsterdam is `ForkCondition::Never`),
            // so the state-gas reservoir is always zero.
            reservoir: 0,
            bytecode_address: to,
            known_bytecode,
            target_address: to,
            caller,
            // `Transfer(value)` matches mainnet CALL semantics: revm's frame init
            // moves `value` from `caller` to `to`, reverting the frame with
            // `InsufficientBalance` when the caller cannot cover it. This
            // directly-built top-level frame bypasses the `CALL` opcode gas site,
            // so no value surcharge or stipend applies: the value transfer is
            // priced by the intrinsic `TX_VALUE_COST`, and account access and
            // creation by `root_call_access_gas` and `NEW_ACCOUNT_COST`.
            value: CallValue::Transfer(call.value),
            scheme: CallScheme::Call,
            is_static: false,
            charged_new_account_state_gas: false,
        }));

        // Wrap the `LocalContext`'s shared memory buffer (a refcount bump, not an
        // allocation) like the mainnet handler's first-frame init. Each call is a
        // fresh call frame, so it must start with empty memory; no frame is live
        // here, and truncating keeps the allocation.
        let ctx = evm.ctx_mut();
        ctx.local().shared_memory_buffer().borrow_mut().clear();
        let mut memory =
            SharedMemory::new_with_buffer(Rc::clone(ctx.local().shared_memory_buffer()));
        memory.set_memory_limit(ctx.cfg().memory_limit());
        let frame_init = FrameInit { depth: 0, memory, frame_input };

        let mut handler = BaseHandler::<
            BaseEvm<DB, I, P>,
            EVMError<DB::Error, BaseTransactionError>,
            EthFrame<EthInterpreter>,
        >::new();
        let mut frame: FrameResult = if evm.inspect {
            handler.inspect_run_exec_loop(evm, frame_init)?
        } else {
            handler.run_exec_loop(evm, frame_init)?
        };

        // A top-level frame's database error is recorded on the context and the
        // interpreter halts with `FatalExternalError`; the root frame is returned
        // directly by `run_exec_loop` without the nested-frame check. Mirror the
        // mainnet `Handler::execution_result` guard so a node-local DB failure
        // raised mid-call propagates as a fatal `Err` instead of being misread as
        // a deterministic call revert on this consensus-critical path.
        take_error::<EVMError<DB::Error, BaseTransactionError>, _>(evm.ctx_mut().error())?;

        // An exceptional halt consumes all of the frame's gas, as it does for a
        // nested frame and for the top frame of a normal transaction. A revert
        // keeps its unused gas.
        if frame.interpreter_result().result.is_halt() {
            frame.gas_mut().spend_all();
        }

        let gas = frame.gas();
        Ok(Eip8130CallFrame {
            success: frame.interpreter_result().result.is_ok(),
            gas_spent: gas.total_gas_spent(),
            refund: gas.refunded(),
            output: frame.interpreter_result().output.clone(),
        })
    }

    fn invalid(error: Eip8130Error) -> Self::Error {
        EVMError::Transaction(Eip8130Executor::tx_error(error))
    }
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use alloy_evm::{
        Evm, EvmError, FromTxWithEncoded,
        precompiles::{DynPrecompile, PrecompileInput, PrecompilesMap},
    };
    use alloy_primitives::{Address, B256, Bytes, U256, address, bytes, keccak256};
    use alloy_sol_types::{SolValue, sol};
    use base_common_consensus::{
        AccountChange, AccountChangeChannel, BaseTxEnvelope, Call, ChangeType, CreateEntry,
        Delegation, Eip8130Constants, Eip8130Contracts, Eip8130Signed, InitialActor, Predeploys,
        SignedAccountChanges, SignedChange, TxEip8130,
    };
    use base_common_precompiles::NonceManagerStorage;
    use base_execution_eip8130::{
        AccountChangeApplier, AccountConfigurationStorage, Eip8130GasSchedule, IntrinsicGas,
    };
    use base_precompile_storage::StorageCtx;
    use k256::ecdsa::SigningKey;
    use revm::{
        Database,
        bytecode::Bytecode,
        context::{BlockEnv, CfgEnv, Context},
        database::InMemoryDB,
        database_interface::DBErrorMarker,
        inspector::NoOpInspector,
        precompile::PrecompileOutput,
        state::{AccountInfo, EvmState},
    };

    use super::*;
    use crate::{
        BaseEvm, BaseSpecId, BaseTransaction, BaseUpgrade, Builder, DefaultBase,
        Eip8130ExecutionMode,
    };

    const CHAIN_ID: u64 = 8453;
    const NOW: u64 = 1_000;
    const BASE_FEE: u64 = 1_000_000_000;
    const BENEFICIARY: Address = address!("0x00000000000000000000000000000000000000bb");

    /// Extra execution gas the warmth probes burn via `JUMPDEST` no-ops (1 gas
    /// each). These probes carry little calldata and do almost no work, so their
    /// `calls` otherwise spend less than the EIP-7623 calldata-floor premium
    /// (`6 gas/token`) — the floor would then clamp both the cold and the warm
    /// outcome to the same value and mask the 2,100-vs-100 SLOAD delta the tests
    /// measure. Padding execution above the floor keeps that delta observable.
    const WARMTH_PROBE_PAD_GAS: u64 = 5_000;

    /// `JUMPDEST * WARMTH_PROBE_PAD_GAS` (each a 1-gas straight-line no-op) then
    /// `PUSH1 0; SLOAD; STOP`. The padding is *deployed* code, not transaction
    /// bytes, so it raises only execution gas — never `tx_payload_cost` — and the
    /// intrinsic breakdown is unchanged from the bare `60005400` probe.
    fn warmth_probe_loader_code() -> Bytes {
        let mut code = vec![0x5bu8; WARMTH_PROBE_PAD_GAS as usize];
        code.extend_from_slice(&[0x60, 0x00, 0x54, 0x00]);
        Bytes::from(code)
    }

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
            valid_after: 0,
            valid_before: 0,
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

    /// Runs `signed` once as `sender` on the read-only `eth_call` path
    /// ([`Eip8130ExecutionMode::Call`]) against `evm`'s current state. The result
    /// reports the metered charge ([`Eip8130Outcome::metered_gas`] of sender
    /// intrinsic plus call-pool gas, gross of refunds). Settlement reports the
    /// reserved budget instead, because gas-limit billing is active wherever
    /// EIP-8130 is, so tests that pin execution-gas accounting measure it here.
    fn metered_call(
        evm: &mut BaseEvm<InMemoryDB, NoOpInspector, PrecompilesMap>,
        signed: &Eip8130Signed,
        sender: Address,
    ) -> ExecutionResult<BaseHaltReason> {
        let mut call = into_base_tx(signed);
        call.base.caller = sender;
        if let Some(parts) = call.eip8130.as_mut() {
            parts.mode = Eip8130ExecutionMode::Call;
        }
        evm.transact_raw(call).expect("eth_call simulation runs").result
    }

    /// Runs `evm` under the Zenith spec, where the Keystore is active.
    fn with_zenith(
        mut evm: BaseEvm<InMemoryDB, NoOpInspector, PrecompilesMap>,
    ) -> BaseEvm<InMemoryDB, NoOpInspector, PrecompilesMap> {
        evm.ctx_mut().cfg.spec = BaseSpecId::new(BaseUpgrade::Zenith);
        evm
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
                CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Everest))
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

    fn seed_account_code(
        evm: &mut BaseEvm<InMemoryDB, NoOpInspector, PrecompilesMap>,
        address: Address,
        code: Bytes,
    ) {
        let db = evm.ctx_mut().journal_mut().db_mut();
        let mut info = db.basic(address).expect("in-memory account read").unwrap_or_default();
        info.code_hash = keccak256(&code);
        info.code = Some(Bytecode::new_raw(code));
        db.insert_account_info(address, info);
    }

    fn journal_account_code(
        evm: &mut BaseEvm<InMemoryDB, NoOpInspector, PrecompilesMap>,
        address: Address,
    ) -> Bytes {
        evm.ctx_mut()
            .journal_mut()
            .load_account_with_code(address)
            .expect("in-memory account code load")
            .info
            .code
            .as_ref()
            .map_or_else(Bytes::new, Bytecode::original_bytes)
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

        // The sender's protocol nonce was bumped and, with no explicit delegation
        // entry, the code-less EOA sender stays code-less.
        let sender_acc = outcome.state.get(&sender).expect("sender in state");
        assert_eq!(sender_acc.info.nonce, 1);
        assert!(sender_acc.info.is_empty_code_hash(), "sender must not be implicitly delegated");
        let ExecutionResult::Success { logs, .. } = &result else { unreachable!() };
        assert!(logs.is_empty(), "no delegation event without a delegation entry");
        assert!(sender_acc.info.balance < initial_balance, "payer should be debited");

        // Fees were routed: base fee to the vault, priority tip to the beneficiary.
        assert!(outcome.state.contains_key(&Predeploys::BASE_FEE_VAULT));
        assert!(outcome.state.contains_key(&BENEFICIARY));
    }

    /// A dispatched call carrying `value` moves that wei from the sender to the
    /// call target, matching mainnet `CALL` value semantics.
    #[test]
    fn eoa_call_transfers_value_to_recipient() {
        let key = signing_key(0x51);
        let sender = eoa_address(&key);
        let recipient = address!("0x00000000000000000000000000000000000000e1");
        let transfer = U256::from(1_000_000u64);

        let mut tx = base_tx();
        tx.calls = vec![vec![Call { to: recipient, value: transfer, data: Bytes::new() }]];
        let signed = eoa_signed(tx, &key);

        let initial_balance = U256::from(10u64).pow(U256::from(18u64));
        let mut evm = evm_with_accounts(initial_balance, sender, &[(recipient, bytes!("00"))]);
        let outcome = evm.transact_raw(into_base_tx(&signed)).expect("value transfer should run");

        assert!(outcome.result.is_success(), "expected success, got {:?}", outcome.result);
        let recipient_acc = outcome.state.get(&recipient).expect("recipient in state");
        assert_eq!(recipient_acc.info.balance, transfer, "recipient credited the call value");
    }

    /// A value-bearing call to an account that does not exist pays
    /// `NEW_ACCOUNT_COST` on top of the same call to an existing account.
    #[test]
    fn value_call_to_new_account_charges_account_creation() {
        let key = signing_key(0x53);
        let sender = eoa_address(&key);
        let recipient = address!("0x00000000000000000000000000000000000000e3");
        // Burns enough gas that both runs bill above the EIP-7623 calldata floor.
        let burner = address!("0x00000000000000000000000000000000000000e5");
        let mut burner_code = vec![0x5bu8; WARMTH_PROBE_PAD_GAS as usize];
        burner_code.push(0x00);
        let burner_code = Bytes::from(burner_code);
        let initial_balance = U256::from(10u64).pow(U256::from(18u64));
        let mut tx = base_tx();
        tx.calls = vec![vec![
            Call { to: recipient, value: U256::from(1u64), data: Bytes::new() },
            Call { to: burner, value: U256::ZERO, data: Bytes::new() },
        ]];
        let signed = eoa_signed(tx, &key);

        let metered = |accounts: &[(Address, Bytes)]| {
            let mut evm = evm_with_accounts(initial_balance, sender, accounts);
            let result = metered_call(&mut evm, &signed, sender);
            assert!(result.is_success(), "expected success, got {result:?}");
            result.tx_gas_used()
        };

        assert_eq!(
            metered(&[(burner, burner_code.clone())])
                - metered(&[(recipient, bytes!("00")), (burner, burner_code)]),
            Eip8130GasSchedule::NEW_ACCOUNT_COST
        );
    }

    /// A new-account charge that does not fit in the remaining call pool reverts
    /// the phase and meters only the gas already consumed. Zeroing the shared
    /// remainder would report the whole pool as spent.
    #[test]
    fn unaffordable_new_account_charge_does_not_burn_the_unspent_pool() {
        let key = signing_key(0x54);
        let sender = eoa_address(&key);
        let recipient = address!("0x00000000000000000000000000000000000000e4");
        let mut tx = base_tx();
        tx.calls = vec![vec![Call { to: recipient, value: U256::from(1u64), data: Bytes::new() }]];

        let intrinsic =
            IntrinsicGas::compute(&eoa_signed(tx.clone(), &key), sender).expect("intrinsic gas");
        tx.gas_limit = intrinsic.sender_intrinsic() + Eip8130GasSchedule::NEW_ACCOUNT_COST - 1;
        let signed = eoa_signed(tx, &key);

        let initial_balance = U256::from(10u64).pow(U256::from(18u64));
        let mut evm = evm_with(initial_balance, sender);
        let outcome = evm.transact_raw(into_base_tx(&signed)).expect("tx should be included");

        assert!(!outcome.result.is_success(), "phase should revert when creation gas does not fit");
        assert_eq!(
            outcome.result.tx_gas_used(),
            signed.tx().gas_limit,
            "gas-limit billing settles the declared limit"
        );

        // The recipient's access is intrinsic and nothing else was spent, so the
        // metered charge is sender-intrinsic gas, raised to the EIP-7623 floor.
        let metered = metered_call(&mut evm_with(initial_balance, sender), &signed, sender);
        assert!(!metered.is_success(), "phase should revert when creation gas does not fit");
        assert_eq!(
            metered.tx_gas_used(),
            intrinsic.sender_intrinsic().max(intrinsic.sender_floor()),
            "unspent call gas must not be metered as spent"
        );
        assert!(metered.tx_gas_used() < signed.tx().gas_limit);
    }

    /// A call whose `value` exceeds the sender's spendable balance (after the
    /// self-pay gas prepay) reverts its phase like any other `CALL`: the
    /// transaction is still included and charged gas, but the recipient is not
    /// credited and the phase's state changes are rolled back.
    #[test]
    fn eoa_call_reverts_when_sender_cannot_cover_value() {
        let key = signing_key(0x52);
        let sender = eoa_address(&key);
        let recipient = address!("0x00000000000000000000000000000000000000e2");

        let mut tx = base_tx();
        // 1e18 wei — far more than the funded balance below.
        let transfer = U256::from(10u64).pow(U256::from(18u64));
        tx.calls = vec![vec![Call { to: recipient, value: transfer, data: Bytes::new() }]];
        let signed = eoa_signed(tx, &key);

        // 1e16 wei covers worst-case gas (gas_limit 1e6 * max_fee 5e9 = 5e15) but
        // is far below the 1e18 call value, so the value transfer cannot succeed.
        let initial_balance = U256::from(10u64).pow(U256::from(16u64));
        let mut evm = evm_with_accounts(initial_balance, sender, &[(recipient, bytes!("00"))]);
        let outcome = evm.transact_raw(into_base_tx(&signed)).expect("tx should be included");

        assert!(!outcome.result.is_success(), "phase should revert on insufficient value");
        let recipient_balance =
            outcome.state.get(&recipient).map_or(U256::ZERO, |acc| acc.info.balance);
        assert!(recipient_balance.is_zero(), "recipient must not be credited on revert");
    }

    /// Cross-chain-replay guard: an EIP-8130 envelope signed for a foreign chain
    /// must be rejected at inclusion, not just at pool admission. The sender and
    /// payer signature hashes commit to the transaction's embedded `chain_id`, so
    /// the same bytes verify on any chain; only `Eip8130Executor::execute`'s
    /// chain-id equality check stops it from advancing the nonce or charging the
    /// balance locally. Regression for the txpool-only `validate_static` gate.
    #[test]
    fn foreign_chain_id_is_rejected_at_inclusion() {
        let key = signing_key(0xa1);
        let sender = eoa_address(&key);
        let mut tx = base_tx();
        tx.chain_id = 1;
        let signed = eoa_signed(tx, &key);
        assert_eq!(signed.tx().chain_id, 1);

        let initial_balance = U256::from(10u64).pow(U256::from(18u64));
        let mut evm = evm_with(initial_balance, sender);
        assert_eq!(evm.ctx().cfg().chain_id(), CHAIN_ID);

        let err = evm.transact_raw(into_base_tx(&signed)).unwrap_err();
        let EVMError::Transaction(BaseTransactionError::Eip8130(reason)) = err else {
            panic!("foreign-chain 8130 tx must be rejected as a validity error, got {err:?}");
        };
        assert!(reason.contains("chain id mismatch"), "unexpected reason: {reason}");
    }

    /// The chain-id guard rejects only mismatches: an envelope whose embedded
    /// `chain_id` equals the local chain still executes normally.
    #[test]
    fn matching_chain_id_still_executes() {
        let key = signing_key(0xa2);
        let sender = eoa_address(&key);
        let mut tx = base_tx();
        tx.chain_id = CHAIN_ID;
        let signed = eoa_signed(tx, &key);

        let initial_balance = U256::from(10u64).pow(U256::from(18u64));
        let mut evm = evm_with(initial_balance, sender);
        let outcome =
            evm.transact_raw(into_base_tx(&signed)).expect("local-chain 8130 tx should execute");

        assert!(outcome.result.is_success());
        let sender_acc = outcome.state.get(&sender).expect("sender in state");
        assert_eq!(sender_acc.info.nonce, 1);
        assert!(sender_acc.info.balance < initial_balance);
    }

    /// A protocol nonce at `u64::MAX` cannot be bumped, so a key-0 transaction
    /// carrying that sequence would stay includable forever. Inclusion rejects it.
    #[test]
    fn saturated_protocol_nonce_is_rejected() {
        let key = signing_key(0x21);
        let sender = eoa_address(&key);
        let signed = eoa_signed(TxEip8130 { nonce_sequence: u64::MAX, ..base_tx() }, &key);
        let mut evm = evm_with(U256::from(10u64).pow(U256::from(18u64)), sender);
        let db = evm.ctx_mut().journal_mut().db_mut();
        let mut info = db.basic(sender).expect("account").unwrap_or_default();
        info.nonce = u64::MAX;
        db.insert_account_info(sender, info);

        let err = evm.transact_raw(into_base_tx(&signed)).unwrap_err();
        assert!(err.to_string().contains("u64::MAX"), "got {err}");
    }

    #[test]
    fn two_dimensional_nonce_is_incremented() {
        let key = signing_key(0x2a);
        let sender = eoa_address(&key);
        let nonce_key = U256::from(7);
        let current_nonce = 3;
        let nonce_slot = NonceManagerStorage::nonce_slot(sender, nonce_key).unwrap();

        let mut tx = base_tx();
        tx.nonce_key = nonce_key;
        tx.nonce_sequence = current_nonce;
        let signed = eoa_signed(tx, &key);
        let storage = [(NonceManagerStorage::ADDRESS, nonce_slot, U256::from(current_nonce))];
        let mut evm = evm_with_accounts_and_storage(
            U256::from(10u64).pow(U256::from(18u64)),
            sender,
            &[],
            &storage,
        );

        let outcome = evm.transact_raw(into_base_tx(&signed)).expect("8130 tx should execute");
        assert!(outcome.result.is_success());

        let nonce_account =
            outcome.state.get(&NonceManagerStorage::ADDRESS).expect("nonce manager in state");
        let stored_nonce =
            nonce_account.storage.get(&nonce_slot).expect("nonce slot updated").present_value;
        assert_eq!(stored_nonce, U256::from(current_nonce + 1));
        assert!(
            outcome.result.logs().iter().all(|log| log.address != NonceManagerStorage::ADDRESS),
            "the protocol nonce increment must not add a receipt log"
        );
    }

    /// A call that reads its own 2D nonce during simulation sees the
    /// post-increment sequence, as it would on inclusion.
    #[test]
    fn simulate_calls_see_the_advanced_two_dimensional_nonce() {
        let key = signing_key(0x2b);
        let sender = eoa_address(&key);
        let reader = address!("0x00000000000000000000000000000000000000ab");
        let nonce_key = 7u8;
        let current_nonce = 3u8;

        // STATICCALL getNonce(sender, nonce_key) and revert unless it returns
        // current_nonce + 1.
        let mut code = vec![0x63];
        code.extend_from_slice(&keccak256("getNonce(address,uint256)")[..4]);
        code.extend_from_slice(&[0x60, 0xe0, 0x1b, 0x60, 0x00, 0x52, 0x73]);
        code.extend_from_slice(sender.as_slice());
        code.extend_from_slice(&[0x60, 0x04, 0x52, 0x60, nonce_key, 0x60, 0x24, 0x52]);
        code.extend_from_slice(&[0x60, 0x20, 0x60, 0x00, 0x60, 0x44, 0x60, 0x00, 0x73]);
        code.extend_from_slice(NonceManagerStorage::ADDRESS.as_slice());
        code.extend_from_slice(&[0x5a, 0xfa, 0x50, 0x60, 0x00, 0x51, 0x60, current_nonce + 1]);
        let ok = u8::try_from(code.len() + 8).unwrap();
        code.extend_from_slice(&[0x14, 0x60, ok, 0x57, 0x60, 0x00, 0x80, 0xfd, 0x5b, 0x00]);

        let mut tx = base_tx();
        tx.nonce_key = U256::from(nonce_key);
        tx.nonce_sequence = current_nonce.into();
        tx.calls = vec![vec![Call { to: reader, value: U256::ZERO, data: Bytes::new() }]];
        let signed = eoa_signed(tx, &key);
        let nonce_slot = NonceManagerStorage::nonce_slot(sender, U256::from(nonce_key)).unwrap();
        let mut evm = evm_with_accounts_and_storage(
            U256::from(10u64).pow(U256::from(18u64)),
            sender,
            &[(reader, code.into())],
            &[(NonceManagerStorage::ADDRESS, nonce_slot, U256::from(current_nonce))],
        );
        let mut call = into_base_tx(&signed);
        call.base.caller = sender;
        if let Some(parts) = call.eip8130.as_mut() {
            parts.mode = Eip8130ExecutionMode::Call;
        }

        let outcome = evm.transact_raw(call).expect("call simulation runs");
        assert!(outcome.result.is_success(), "expected success, got {:?}", outcome.result);
    }

    #[test]
    fn transact_raw_rejects_delegation_over_ordinary_sender_code_without_replacing_it() {
        let key = signing_key(0x23);
        let sender = eoa_address(&key);
        let ordinary_code = bytes!("60006000");
        let target = address!("0x00000000000000000000000000000000000000dd");
        let mut tx = base_tx();
        tx.sender = Some(sender);
        tx.account_changes = vec![AccountChange::Delegation(Delegation { target })];
        let signed = configured_signed(tx, &key);
        let mut evm = evm_with(U256::from(10u64).pow(U256::from(18u64)), sender);
        seed_account_code(&mut evm, sender, ordinary_code.clone());

        let error = evm.transact_raw(into_base_tx(&signed)).unwrap_err();

        let EVMError::Transaction(BaseTransactionError::Eip8130(reason)) = error else {
            panic!("ordinary sender code must reject delegation inclusion, got {error:?}");
        };
        assert!(
            reason.contains("delegation cannot replace non-delegation code"),
            "unexpected delegation rejection: {reason}"
        );
        assert_eq!(journal_account_code(&mut evm, sender), ordinary_code);
    }

    /// Inclusion validates the nonce before authorizing or applying account
    /// changes, so a stale transaction is rejected for its nonce even when an
    /// account change would also fail.
    #[test]
    fn stale_nonce_is_rejected_before_account_changes() {
        let key = signing_key(0x25);
        let sender = eoa_address(&key);
        let ordinary_code = bytes!("60006000");
        let mut tx = base_tx();
        tx.sender = Some(sender);
        tx.account_changes = vec![AccountChange::Delegation(Delegation {
            target: address!("0x00000000000000000000000000000000000000dd"),
        })];
        let signed = configured_signed(tx, &key);
        let mut evm = evm_with(U256::from(10u64).pow(U256::from(18u64)), sender);
        seed_account_code(&mut evm, sender, ordinary_code.clone());
        let db = evm.ctx_mut().journal_mut().db_mut();
        let mut info = db.basic(sender).expect("account").unwrap_or_default();
        info.nonce = 1;
        db.insert_account_info(sender, info);

        let error = evm.transact_raw(into_base_tx(&signed)).unwrap_err();

        let EVMError::Transaction(BaseTransactionError::Eip8130(reason)) = error else {
            panic!("a stale nonce must reject inclusion, got {error:?}");
        };
        assert!(reason.contains("below the channel nonce"), "unexpected rejection: {reason}");
        assert_eq!(journal_account_code(&mut evm, sender), ordinary_code);
    }

    /// A delegation must consume the protocol nonce: inclusion accepts it on
    /// nonce key 0, leaving the delegated account with a non-zero nonce, and
    /// rejects it on a 2D nonce lane or nonce-free.
    #[test]
    fn delegation_requires_protocol_nonce() {
        let key = signing_key(0x27);
        let sender = eoa_address(&key);
        let target = address!("0x00000000000000000000000000000000000000dd");
        let transact = |nonce_key: U256| {
            let mut tx = base_tx();
            tx.nonce_key = nonce_key;
            tx.account_changes = vec![AccountChange::Delegation(Delegation { target })];
            let signed = eoa_signed(tx, &key);
            let mut evm = evm_with(U256::from(10u64).pow(U256::from(18u64)), sender);
            evm.transact_raw(into_base_tx(&signed))
        };

        let outcome = transact(U256::ZERO).expect("delegation on the protocol nonce executes");
        assert!(outcome.result.is_success());
        let sender_acc = outcome.state.get(&sender).expect("sender in state");
        assert_eq!(sender_acc.info.nonce, 1);
        assert!(!sender_acc.info.is_empty_code_hash(), "delegation indicator must be installed");

        for nonce_key in [U256::from(7), Eip8130Constants::NONCE_KEY_MAX] {
            let error = transact(nonce_key).unwrap_err();
            let EVMError::Transaction(BaseTransactionError::Eip8130(reason)) = error else {
                panic!("a delegation off the protocol nonce must reject inclusion, got {error:?}");
            };
            assert!(
                reason.contains("must use the protocol nonce"),
                "unexpected rejection: {reason}"
            );
        }
    }

    #[test]
    fn explicit_zero_delegation_remains_cleared() {
        let key = signing_key(0x24);
        let sender = eoa_address(&key);
        let mut tx = base_tx();
        tx.account_changes = vec![AccountChange::Delegation(Delegation { target: Address::ZERO })];
        let signed = eoa_signed(tx, &key);
        let mut evm = evm_with(U256::from(10u64).pow(U256::from(18u64)), sender);
        seed_account_code(
            &mut evm,
            sender,
            Bytecode::new_eip7702(address!("0x00000000000000000000000000000000000000dd"))
                .original_bytes(),
        );

        let outcome = evm.transact_raw(into_base_tx(&signed)).expect("clear should execute");
        let sender_acc = outcome.state.get(&sender).expect("sender in state");
        assert!(sender_acc.info.is_empty_code_hash(), "explicit zero target must remain cleared");

        let ExecutionResult::Success { logs, .. } = &outcome.result else {
            panic!("expected successful clear, got {:?}", outcome.result);
        };
        assert!(logs.is_empty(), "delegation updates emit no protocol logs");
    }

    #[test]
    fn simulate_rejects_delegation_over_ordinary_sender_code_and_rolls_back() {
        let key = signing_key(0x25);
        let sender = eoa_address(&key);
        let ordinary_code = bytes!("60016000");
        let target = address!("0x00000000000000000000000000000000000000dd");
        let mut tx = base_tx();
        tx.sender = Some(sender);
        tx.account_changes = vec![AccountChange::Delegation(Delegation { target })];
        let signed = configured_signed(tx, &key);
        let mut evm = evm_with(U256::from(10u64).pow(U256::from(18u64)), sender);
        seed_account_code(&mut evm, sender, ordinary_code.clone());
        evm.ctx_mut().tx = into_base_tx(&signed);
        evm.ctx_mut().tx.base.caller = sender;

        let error = Eip8130Executor::simulate(&mut evm).unwrap_err();

        let EVMError::Transaction(BaseTransactionError::Eip8130(reason)) = error else {
            panic!("ordinary sender code must reject delegation simulation, got {error:?}");
        };
        assert!(
            reason.contains("delegation cannot replace non-delegation code"),
            "unexpected delegation rejection: {reason}"
        );
        assert_eq!(journal_account_code(&mut evm, sender), ordinary_code);
    }

    #[test]
    fn simulate_explicit_zero_delegation_clears_without_logs() {
        let key = signing_key(0x26);
        let sender = eoa_address(&key);
        let mut tx = base_tx();
        tx.account_changes = vec![AccountChange::Delegation(Delegation { target: Address::ZERO })];
        let signed = eoa_signed(tx, &key);
        let mut evm = evm_with(U256::from(10u64).pow(U256::from(18u64)), sender);
        seed_account_code(
            &mut evm,
            sender,
            Bytecode::new_eip7702(address!("0x00000000000000000000000000000000000000dd"))
                .original_bytes(),
        );
        evm.ctx_mut().tx = into_base_tx(&signed);
        evm.ctx_mut().tx.base.caller = sender;

        let result = Eip8130Executor::simulate(&mut evm).expect("clear should simulate");
        let ExecutionResult::Success { logs, .. } = result else {
            panic!("expected successful simulation, got {result:?}");
        };
        assert!(logs.is_empty(), "delegation updates emit no protocol logs");
    }

    #[test]
    fn simulate_estimate_covers_execution_gas_without_a_signature() {
        let key = signing_key(0x77);
        let sender = eoa_address(&key);
        let target = address!("0x00000000000000000000000000000000000000c5");

        let mut tx = base_tx();
        tx.calls = vec![vec![Call { to: target, value: U256::ZERO, data: Bytes::new() }]];
        let signed = eoa_signed(tx, &key);
        let initial = U256::from(10u64).pow(U256::from(18u64));

        // Reference: the metered charge of the fully-signed transaction (the
        // `eth_call` path) and its settled charge, then estimate over the same
        // shape with the sender supplied as `from` (the signature is never
        // recovered on the simulate path).
        let charges = |zenith: bool| {
            let fresh = || {
                let evm = evm_with_accounts(initial, sender, &[(target, bytes!("00"))]);
                if zenith { with_zenith(evm) } else { evm }
            };
            let settled = fresh()
                .transact_raw(into_base_tx(&signed))
                .expect("tx should execute")
                .result
                .gas()
                .tx_gas_used();
            let metered = metered_call(&mut fresh(), &signed, sender).tx_gas_used();
            let mut evm_sim = fresh();
            evm_sim.ctx_mut().tx = into_base_tx(&signed);
            evm_sim.ctx_mut().tx.base.caller = sender;
            let sim_result =
                Eip8130Executor::simulate(&mut evm_sim).expect("estimation should succeed");
            assert!(sim_result.is_success(), "estimation should report success");
            (settled, metered, sim_result.tx_gas_used())
        };

        // The estimate is a gas *limit* that must cover the metered execution
        // charge. This transaction calls a STOP contract (no nested calls, no
        // SSTORE/SELFDESTRUCT), so it loses no gas to EIP-150 forwarding and earns
        // no refund. Intrinsic gas reads no state, so with and without the
        // Keystore the estimate is the metered charge exactly. Gas-limit billing
        // settles the declared limit rather than the metered charge.
        for zenith in [false, true] {
            let (settled, metered, sim_gas) = charges(zenith);
            assert!(sim_gas > 0, "estimated gas should be positive");
            assert_eq!(sim_gas, metered, "zenith = {zenith}");
            assert_eq!(
                settled,
                base_tx().gas_limit,
                "gas-limit billing must settle a self-paid transaction at its declared limit",
            );
        }

        // Estimation never commits: a fresh execution after it still bumps the
        // nonce from zero, proving no nonce was consumed by the simulation.
        let mut evm_after = evm_with_accounts(initial, sender, &[(target, bytes!("00"))]);
        let after = evm_after.transact_raw(into_base_tx(&signed)).expect("tx should execute");
        assert_eq!(after.state.get(&sender).expect("sender").info.nonce, 1);
    }

    // ---- Gas-limit billing (BaseFeature::GasLimitBilling) ----

    /// Operator fee scalar seeded into the `L1Block` predeploy for the settlement
    /// tests, so the operator fee is gas-proportional and distinguishes a charge
    /// on metered gas from one on the reserved budget.
    const OPERATOR_FEE_SCALAR: u32 = 1;

    /// `L1Block` storage entry carrying [`OPERATOR_FEE_SCALAR`] (and a zero
    /// operator-fee constant) in the packed operator-fee scalars slot.
    fn operator_fee_storage() -> (Address, U256, U256) {
        let mut slot = [0u8; 32];
        slot[L1BlockInfo::OPERATOR_FEE_SCALAR_OFFSET..L1BlockInfo::OPERATOR_FEE_SCALAR_OFFSET + 4]
            .copy_from_slice(&OPERATOR_FEE_SCALAR.to_be_bytes());
        (
            Predeploys::L1_BLOCK_INFO,
            L1BlockInfo::OPERATOR_FEE_SCALARS_SLOT,
            U256::from_be_bytes(slot),
        )
    }

    /// Operator fee owed on `gas` under [`operator_fee_storage`] (post-Jovian
    /// `gas * scalar * 100` wei).
    fn operator_fee(gas: u64) -> U256 {
        U256::from(gas)
            * U256::from(OPERATOR_FEE_SCALAR)
            * U256::from(L1BlockInfo::OPERATOR_FEE_JOVIAN_MULTIPLIER)
    }

    /// Funding given to the payer in the settlement tests.
    fn settlement_funding() -> U256 {
        U256::from(10u64).pow(U256::from(18u64))
    }

    /// An EVM for the settlement tests: `payer` funded, `contracts` deployed, the
    /// operator fee scalar seeded, and the spec set to `upgrade`.
    fn settlement_evm(
        upgrade: BaseUpgrade,
        payer: Address,
        contracts: &[(Address, Bytes)],
    ) -> BaseEvm<InMemoryDB, NoOpInspector, PrecompilesMap> {
        let mut evm = evm_with_accounts_and_storage(
            settlement_funding(),
            payer,
            contracts,
            &[operator_fee_storage()],
        );
        evm.ctx_mut().cfg.spec = BaseSpecId::new(upgrade);
        evm
    }

    /// What settling one EIP-8130 transaction moved: the gas the result reports,
    /// the payer's debit, each fee recipient's credit, and the resulting state.
    struct Settlement {
        success: bool,
        gas_used: u64,
        /// What the executor published for the builder's events.
        metered: MeteredGas,
        paid: U256,
        state: EvmState,
        tip: U256,
        burned: U256,
        l1_fee: U256,
        operator_fee: U256,
    }

    impl Settlement {
        /// Total credited to fee recipients; equals `paid` when settlement neither
        /// mints nor burns.
        fn received(&self) -> U256 {
            self.tip + self.burned + self.l1_fee + self.operator_fee
        }

        /// The protocol nonce of `address` after the transaction.
        fn nonce(&self, address: Address) -> u64 {
            self.state.get(&address).map(|acc| acc.info.nonce).unwrap_or_default()
        }
    }

    /// Executes the fully-signed `signed` under `upgrade`, paid by `payer`.
    fn settle(
        upgrade: BaseUpgrade,
        signed: &Eip8130Signed,
        payer: Address,
        contracts: &[(Address, Bytes)],
    ) -> Settlement {
        settle_on(&mut settlement_evm(upgrade, payer, contracts), signed, payer)
    }

    /// Executes the fully-signed `signed` on `evm`, a [`settlement_evm`] that
    /// funded `payer`.
    fn settle_on(
        evm: &mut BaseEvm<InMemoryDB, NoOpInspector, PrecompilesMap>,
        signed: &Eip8130Signed,
        payer: Address,
    ) -> Settlement {
        let outcome = evm.transact_raw(into_base_tx(signed)).expect("tx should execute");
        let metered = MeteredGas::take().expect("a settled transaction publishes its metered gas");
        let balance = |address: Address| {
            outcome.state.get(&address).map(|acc| acc.info.balance).unwrap_or_default()
        };
        Settlement {
            success: outcome.result.is_success(),
            gas_used: outcome.result.tx_gas_used(),
            metered,
            paid: settlement_funding() - balance(payer),
            tip: balance(BENEFICIARY),
            burned: balance(Predeploys::BASE_FEE_VAULT),
            l1_fee: balance(Predeploys::L1_FEE_VAULT),
            operator_fee: balance(Predeploys::OPERATOR_FEE_VAULT),
            state: outcome.state,
        }
    }

    /// Estimates `signed` under `upgrade` from `from` (no signature recovered),
    /// with `payer` funded; returns the recommended gas limit.
    fn estimate(
        upgrade: BaseUpgrade,
        signed: &Eip8130Signed,
        from: Address,
        payer: Address,
        contracts: &[(Address, Bytes)],
    ) -> u64 {
        let mut evm = settlement_evm(upgrade, payer, contracts);
        evm.ctx_mut().tx = into_base_tx(signed);
        evm.ctx_mut().tx.base.caller = from;
        if let Some(parts) = evm.ctx_mut().tx.eip8130.as_mut() {
            parts.mode = Eip8130ExecutionMode::Simulate;
        }
        let result = Eip8130Executor::simulate(&mut evm).expect("estimate succeeds");
        assert!(result.is_success(), "estimate should report success, got {result:?}");
        result.tx_gas_used()
    }

    /// Under gas-limit billing, active wherever EIP-8130 is, a self-paid
    /// transaction is billed and reports its whole `gas_limit`, however little the
    /// `calls` consume: base-fee burn, coinbase tip and operator fee are all
    /// computed on the limit, and nothing is minted or burned.
    #[test]
    fn gas_limit_billing_settles_self_pay_at_the_declared_limit() {
        let key = signing_key(0x78);
        let sender = eoa_address(&key);
        let target = address!("0x00000000000000000000000000000000000000c6");
        let contracts = [(target, bytes!("00"))];

        let tx = TxEip8130 {
            calls: vec![vec![Call { to: target, value: U256::ZERO, data: Bytes::new() }]],
            ..base_tx()
        };
        let gas_limit = tx.gas_limit;
        let priority_fee = tx.max_priority_fee_per_gas;
        let effective = u128::from(BASE_FEE) + priority_fee;
        assert!(effective <= tx.max_fee_per_gas, "fixture must not be capped by max_fee");
        let signed = eoa_signed(tx, &key);

        let metered = metered_call(
            &mut settlement_evm(BaseUpgrade::Everest, sender, &contracts),
            &signed,
            sender,
        );
        assert!(metered.is_success());
        assert!(metered.tx_gas_used() < gas_limit, "fixture must leave gas unused");

        let billed = settle(BaseUpgrade::Everest, &signed, sender, &contracts);
        assert!(billed.success);
        assert_eq!(billed.gas_used, gas_limit, "result must report the reserved budget");
        // The metered gas survives for telemetry; a self-paid transaction reserves no
        // payer authentication, so the reserved budget is the limit alone.
        assert_eq!(
            billed.metered,
            MeteredGas { gas_used: metered.tx_gas_used(), payer_auth_gas_reserved: Some(0) }
        );
        assert_eq!(billed.tip, U256::from(priority_fee) * U256::from(gas_limit));
        assert_eq!(billed.burned, U256::from(BASE_FEE) * U256::from(gas_limit));
        assert_eq!(billed.operator_fee, operator_fee(gas_limit));
        assert_eq!(
            billed.paid,
            U256::from(effective) * U256::from(gas_limit) + billed.l1_fee + billed.operator_fee
        );
        assert_eq!(billed.paid, billed.received(), "billing must neither mint nor burn");
    }

    /// A transaction whose only call reverts (the validity-transaction shape) is
    /// still included, and under gas-limit billing it settles its declared budget
    /// with the full priority fee going to the sequencer rather than the metered
    /// cost of the early revert.
    #[test]
    fn gas_limit_billing_settles_a_reverting_call_at_the_declared_limit() {
        let key = signing_key(0x79);
        let sender = eoa_address(&key);
        let target = address!("0x00000000000000000000000000000000000000c7");
        // PUSH1 0, PUSH1 0, REVERT.
        let contracts = [(target, bytes!("60006000fd"))];

        let tx = TxEip8130 {
            calls: vec![vec![Call { to: target, value: U256::ZERO, data: Bytes::new() }]],
            ..base_tx()
        };
        let gas_limit = tx.gas_limit;
        let priority_fee = tx.max_priority_fee_per_gas;
        let signed = eoa_signed(tx, &key);

        let metered = metered_call(
            &mut settlement_evm(BaseUpgrade::Everest, sender, &contracts),
            &signed,
            sender,
        );
        assert!(!metered.is_success(), "the call must revert");
        assert!(metered.tx_gas_used() < gas_limit, "the revert must leave gas unused");

        let billed = settle(BaseUpgrade::Everest, &signed, sender, &contracts);
        assert!(!billed.success, "the call must still revert");
        assert_eq!(billed.gas_used, gas_limit, "result must report the reserved budget");
        assert_eq!(
            billed.metered,
            MeteredGas { gas_used: metered.tx_gas_used(), payer_auth_gas_reserved: Some(0) }
        );
        assert_eq!(billed.tip, U256::from(priority_fee) * U256::from(gas_limit));
        assert_eq!(billed.burned, U256::from(BASE_FEE) * U256::from(gas_limit));
        assert_eq!(billed.operator_fee, operator_fee(gas_limit));
        assert_eq!(billed.paid, billed.received(), "billing must neither mint nor burn");
    }

    /// Under gas-limit billing the estimate excludes payer authentication, because
    /// settlement adds `payer_auth_cost` on top of whatever limit the sender declares:
    /// a sponsored transaction settles its declared limit plus `payer_auth_cost` once,
    /// rather than paying for payer authentication twice.
    #[test]
    fn gas_limit_billing_estimate_excludes_payer_auth_so_it_is_billed_once() {
        let key = signing_key(0x38);
        let sender = eoa_address(&key);
        let payer_key = signing_key(0x39);
        let payer = eoa_address(&payer_key);
        let target = address!("0x00000000000000000000000000000000000000ca");
        let contracts = [(target, bytes!("00"))];

        let tx = TxEip8130 {
            payer: Some(Eip8130Constants::OPEN_PAYER),
            calls: vec![vec![Call { to: target, value: U256::ZERO, data: Bytes::new() }]],
            ..base_tx()
        };
        let effective = u128::from(BASE_FEE) + tx.max_priority_fee_per_gas;
        let intrinsic_of =
            |signed: &Eip8130Signed| IntrinsicGas::compute(signed, sender).expect("intrinsic gas");

        // Estimate the way `eth_estimateGas` does: nobody has signed yet, so both
        // auth blobs are 65-byte placeholders of all-non-zero bytes (the RPC's
        // `STUB_AUTH_FILL`). No real signature costs more.
        let placeholder = Bytes::from([0xff_u8; 65]);
        let unsigned = Eip8130Signed::new(tx.clone(), placeholder.clone(), placeholder);
        let estimated = intrinsic_of(&unsigned);
        assert!(
            estimated.payer_auth_cost > 0,
            "a sponsored transaction meters payer authentication"
        );

        // The estimate is the sender budget alone: sender-intrinsic gas (which
        // already holds the STOP target's cold access as the first call), raised
        // to the sender's EIP-7623 floor, with no payer authentication folded in.
        let estimate_gas = estimate(BaseUpgrade::Everest, &unsigned, sender, payer, &contracts);
        assert_eq!(
            estimate_gas,
            estimated.sender_intrinsic().max(estimated.sender_floor()),
            "the estimate must exclude `payer_auth_cost` under gas-limit billing",
        );

        // Sign at exactly `gas_limit = estimate`, as a wallet would, with no slack.
        let declared_tx = TxEip8130 { gas_limit: estimate_gas, ..tx };
        let declared = Eip8130Signed::new(
            declared_tx.clone(),
            eoa_sig(&key, declared_tx.sender_signature_hash()),
            eoa_sig(&payer_key, declared_tx.payer_signature_hash(sender)),
        );
        let actual = intrinsic_of(&declared);
        // Real signatures never cost more than the placeholders, so the estimate still
        // clears the sender floor and leaves the calls at least the searched pool.
        assert!(actual.sender_intrinsic() <= estimated.sender_intrinsic());
        assert!(actual.sender_floor() <= estimated.sender_floor());

        let billed = settle(BaseUpgrade::Everest, &declared, payer, &contracts);
        assert!(billed.success, "`gas_limit = estimate` must be a sufficient limit");
        // Billed once: the declared limit plus the real payer authentication.
        assert_eq!(billed.gas_used, estimate_gas + actual.payer_auth_cost);
        assert_eq!(
            billed.paid,
            U256::from(effective) * U256::from(billed.gas_used)
                + billed.l1_fee
                + operator_fee(billed.gas_used)
        );
        assert_eq!(billed.paid, billed.received(), "billing must neither mint nor burn");
    }

    /// A sponsored transaction publishes, for the builder's events, the gas it would have
    /// been billed without gas-limit billing (payer authentication included) and the reserved
    /// payer authentication (`payer_auth_cost`) that the bill adds on top of the declared limit,
    /// so `gas_used == gas_limit + payer_auth_gas_reserved`.
    #[test]
    fn gas_limit_billing_publishes_metered_gas_and_reserved_payer_auth_for_a_sponsored_tx() {
        let key = signing_key(0x3a);
        let sender = eoa_address(&key);
        let payer_key = signing_key(0x3b);
        let payer = eoa_address(&payer_key);
        let target = address!("0x00000000000000000000000000000000000000cb");
        let contracts = [(target, bytes!("00"))];

        let tx = TxEip8130 {
            payer: Some(Eip8130Constants::OPEN_PAYER),
            calls: vec![vec![Call { to: target, value: U256::ZERO, data: Bytes::new() }]],
            ..base_tx()
        };
        let gas_limit = tx.gas_limit;
        let sender_auth = eoa_sig(&key, tx.sender_signature_hash());
        let payer_auth = eoa_sig(&payer_key, tx.payer_signature_hash(sender));
        let signed = Eip8130Signed::new(tx, sender_auth, payer_auth);
        let payer_auth_cost = IntrinsicGas::payer_auth_cost_of(&signed).expect("payer auth cost");
        assert!(payer_auth_cost > 0, "a sponsored transaction reserves payer authentication");

        let metered = metered_call(
            &mut settlement_evm(BaseUpgrade::Everest, payer, &contracts),
            &signed,
            sender,
        );
        assert!(metered.is_success());
        assert!(metered.tx_gas_used() < gas_limit, "the fixture must leave gas unused");

        let billed = settle(BaseUpgrade::Everest, &signed, payer, &contracts);
        assert!(billed.success);
        assert_eq!(billed.gas_used, gas_limit + payer_auth_cost);
        assert_eq!(
            billed.metered,
            MeteredGas {
                gas_used: metered.tx_gas_used(),
                payer_auth_gas_reserved: Some(payer_auth_cost)
            }
        );
    }

    /// Records the gas spent by the last frame to close, which for an inspected
    /// EIP-8130 transaction is the synthetic transaction root.
    #[derive(Debug, Default)]
    struct RootGasSpent(Option<u64>);

    impl<CTX> Inspector<CTX> for RootGasSpent {
        fn call_end(&mut self, _: &mut CTX, _: &CallInputs, outcome: &mut CallOutcome) {
            self.0 = Some(outcome.result.gas.total_gas_spent());
        }
    }

    /// Under gas-limit billing a sponsored transaction settles
    /// `gas_limit + payer_auth_cost`, more than the sender's `gas_limit`. The trace
    /// root reports that billed gas, as the receipt does, instead of capping it
    /// at `gas_limit`.
    #[test]
    fn gas_limit_billing_trace_root_reports_the_billed_gas_of_a_sponsored_tx() {
        let key = signing_key(0x3c);
        let sender = eoa_address(&key);
        let payer_key = signing_key(0x3d);
        let payer = eoa_address(&payer_key);
        let target = address!("0x00000000000000000000000000000000000000cc");
        let contracts = [(target, bytes!("00"))];

        let tx = TxEip8130 {
            payer: Some(Eip8130Constants::OPEN_PAYER),
            calls: vec![vec![Call { to: target, value: U256::ZERO, data: Bytes::new() }]],
            ..base_tx()
        };
        let gas_limit = tx.gas_limit;
        let sender_auth = eoa_sig(&key, tx.sender_signature_hash());
        let payer_auth = eoa_sig(&payer_key, tx.payer_signature_hash(sender));
        let signed = Eip8130Signed::new(tx, sender_auth, payer_auth);
        let payer_auth_cost = IntrinsicGas::payer_auth_cost_of(&signed).expect("payer auth cost");
        assert!(payer_auth_cost > 0, "a sponsored transaction reserves payer authentication");

        let mut evm = settlement_evm(BaseUpgrade::Everest, payer, &contracts)
            .with_inspector(RootGasSpent::default());
        evm.set_inspector_enabled(true);
        let outcome = evm.transact_raw(into_base_tx(&signed)).expect("tx should execute");
        let _ = MeteredGas::take();
        assert!(outcome.result.is_success());
        assert_eq!(outcome.result.tx_gas_used(), gas_limit + payer_auth_cost);
        assert_eq!(
            evm.components().1.0,
            Some(gas_limit + payer_auth_cost),
            "the trace root must report the billed gas, not the sender's gas limit"
        );
    }

    /// With fee charging disabled, as `eth_call`, `eth_estimateGas` and
    /// unvalidated `eth_simulateV1` run, a self-paid transaction under gas-limit
    /// billing still consumes its nonce and moves its `value`, but pays no fee
    /// (nothing reaches the vaults or the coinbase) and reports the gas it
    /// metered rather than the reserved budget the same transaction is billed
    /// when fees are charged. The figure equals the `Call`-mode simulation's only
    /// because this fixture earns no refund: fee-free `execute` is net of the
    /// capped refund like standard `eth_call`, while `simulate` in `Call` mode is
    /// gross (see `fee_charge_disabled_execution_nets_the_capped_refund`).
    #[test]
    fn fee_charge_disabled_execution_meters_gas_and_moves_no_fees() {
        let key = signing_key(0x7a);
        let sender = eoa_address(&key);
        let target = address!("0x00000000000000000000000000000000000000c8");
        let contracts = [(target, bytes!("00"))];
        let value = U256::from(7);

        let tx = TxEip8130 {
            calls: vec![vec![Call { to: target, value, data: Bytes::new() }]],
            ..base_tx()
        };
        let gas_limit = tx.gas_limit;
        let signed = eoa_signed(tx, &key);

        let metered = metered_call(
            &mut settlement_evm(BaseUpgrade::Everest, sender, &contracts),
            &signed,
            sender,
        );
        assert!(metered.is_success());
        let metered_gas = metered.tx_gas_used();
        assert!(metered_gas < gas_limit, "fixture must leave gas unused");

        let mut fee_free = settlement_evm(BaseUpgrade::Everest, sender, &contracts);
        fee_free.ctx_mut().cfg.disable_fee_charge = true;
        let unbilled = settle_on(&mut fee_free, &signed, sender);
        assert!(unbilled.success);
        assert_eq!(unbilled.gas_used, metered_gas, "must report metered gas, not the limit");
        assert_eq!(unbilled.paid, value, "only the call's value may leave the sender");
        assert_eq!(unbilled.received(), U256::ZERO, "no fee may reach any recipient");
        assert_eq!(unbilled.nonce(sender), 1, "the protocol nonce is still consumed");

        let billed = settle(BaseUpgrade::Everest, &signed, sender, &contracts);
        assert!(billed.success);
        assert_eq!(billed.gas_used, gas_limit, "fees on: the reserved budget is billed");
        assert!(billed.paid > value, "fees on: the sender pays for the reservation");
        assert_eq!(billed.paid - value, billed.received());
        assert_eq!(billed.nonce(sender), 1);
    }

    /// A sponsored transaction executed with fee charging disabled leaves the
    /// payer's balance untouched and reports the metered gas including payer
    /// authentication, below the `gas_limit + payer_auth_cost` budget it is billed
    /// when fees are charged; the sender's nonce is consumed either way.
    #[test]
    fn fee_charge_disabled_execution_does_not_debit_a_sponsoring_payer() {
        let key = signing_key(0x7b);
        let sender = eoa_address(&key);
        let payer_key = signing_key(0x7c);
        let payer = eoa_address(&payer_key);
        let target = address!("0x00000000000000000000000000000000000000c9");
        let contracts = [(target, bytes!("00"))];

        let tx = TxEip8130 {
            payer: Some(Eip8130Constants::OPEN_PAYER),
            calls: vec![vec![Call { to: target, value: U256::ZERO, data: Bytes::new() }]],
            ..base_tx()
        };
        let gas_limit = tx.gas_limit;
        let sender_auth = eoa_sig(&key, tx.sender_signature_hash());
        let payer_auth = eoa_sig(&payer_key, tx.payer_signature_hash(sender));
        let signed = Eip8130Signed::new(tx, sender_auth, payer_auth);
        let intrinsic = IntrinsicGas::compute(&signed, sender).expect("intrinsic gas");
        assert!(
            intrinsic.payer_auth_cost > 0,
            "a sponsored transaction meters payer authentication"
        );

        let metered = metered_call(
            &mut settlement_evm(BaseUpgrade::Everest, payer, &contracts),
            &signed,
            sender,
        );
        assert!(metered.is_success());
        let metered_gas = metered.tx_gas_used();
        assert!(
            metered_gas < gas_limit + intrinsic.payer_auth_cost,
            "fixture must leave gas unused"
        );

        let mut fee_free = settlement_evm(BaseUpgrade::Everest, payer, &contracts);
        fee_free.ctx_mut().cfg.disable_fee_charge = true;
        let unbilled = settle_on(&mut fee_free, &signed, payer);
        assert!(unbilled.success);
        assert_eq!(unbilled.gas_used, metered_gas, "must report metered gas, not the budget");
        assert_eq!(unbilled.paid, U256::ZERO, "the payer must not be debited");
        assert_eq!(unbilled.received(), U256::ZERO, "no fee may reach any recipient");
        assert_eq!(unbilled.nonce(payer), 0, "a sponsoring payer's nonce is never touched");
        assert_eq!(unbilled.nonce(sender), 1, "the sender's protocol nonce is consumed");

        let billed = settle(BaseUpgrade::Everest, &signed, payer, &contracts);
        assert!(billed.success);
        assert_eq!(billed.gas_used, gas_limit + intrinsic.payer_auth_cost);
        assert_eq!(billed.paid, billed.received());
        assert!(billed.paid > U256::ZERO);
    }

    /// A fee-free execution whose call earns an EIP-3529 refund reports the gas
    /// net of the capped refund, as standard `eth_call` does, below the gross
    /// spend the `Call`-mode simulation reports (the pool the call needs) by
    /// exactly that refund; with fees on the reserved limit is billed regardless.
    #[test]
    fn fee_charge_disabled_execution_nets_the_capped_refund() {
        let key = signing_key(0x7d);
        let sender = eoa_address(&key);
        let target = address!("0x00000000000000000000000000000000000000cb");
        // `JUMPDEST * WARMTH_PROBE_PAD_GAS` keeps the net spend above the EIP-7623
        // floor, then `PUSH1 1 PUSH1 0 SSTORE` (slot 0: 0 -> 1) and
        // `PUSH1 0 PUSH1 0 SSTORE` (slot 0: 1 -> 0) `STOP`.
        let mut code = vec![0x5bu8; WARMTH_PROBE_PAD_GAS as usize];
        code.extend_from_slice(&[0x60, 0x01, 0x60, 0x00, 0x55, 0x60, 0x00, 0x60, 0x00, 0x55, 0x00]);
        let contracts = [(target, Bytes::from(code))];

        // EIP-2929/EIP-3529 schedule, by hand. The first SSTORE pays the cold slot
        // access plus SSTORE_SET (original 0, writing non-zero); the second finds a
        // dirty slot and pays the warm SLOAD rate, and restoring the original zero
        // refunds SSTORE_SET minus that warm rate. The cold account access of
        // `target` is intrinsic, as the first call.
        const PUSH1_GAS: u64 = 3;
        const COLD_SLOAD_COST: u64 = 2_100;
        const SSTORE_SET: u64 = 20_000;
        const CALL_GAS_SPENT: u64 = WARMTH_PROBE_PAD_GAS
            + 4 * PUSH1_GAS
            + COLD_SLOAD_COST
            + SSTORE_SET
            + WARM_STORAGE_READ_COST;
        const RESTORE_ZERO_REFUND: u64 = SSTORE_SET - WARM_STORAGE_READ_COST;
        const EIP3529_REFUND_QUOTIENT: u64 = 5;

        let tx = TxEip8130 {
            calls: vec![vec![Call { to: target, value: U256::ZERO, data: Bytes::new() }]],
            ..base_tx()
        };
        let gas_limit = tx.gas_limit;
        let signed = eoa_signed(tx, &key);
        let intrinsic = IntrinsicGas::compute(&signed, sender).expect("intrinsic gas");
        let gross = intrinsic.sender_intrinsic() + CALL_GAS_SPENT;
        let capped_refund = RESTORE_ZERO_REFUND.min(gross / EIP3529_REFUND_QUOTIENT);
        let net = gross - capped_refund;
        assert!(capped_refund > 0, "fixture must earn a refund");
        assert!(net > intrinsic.sender_floor(), "the EIP-7623 floor must not mask the refund");

        let metered = metered_call(
            &mut settlement_evm(BaseUpgrade::Everest, sender, &contracts),
            &signed,
            sender,
        );
        assert!(metered.is_success(), "expected success, got {metered:?}");
        assert_eq!(metered.tx_gas_used(), gross, "`Call`-mode simulation is gross of refund");

        let mut fee_free = settlement_evm(BaseUpgrade::Everest, sender, &contracts);
        fee_free.ctx_mut().cfg.disable_fee_charge = true;
        let unbilled = settle_on(&mut fee_free, &signed, sender);
        assert!(unbilled.success);
        assert_eq!(unbilled.gas_used, net, "must report metered gas net of the capped refund");
        assert_eq!(metered.tx_gas_used() - unbilled.gas_used, capped_refund);
        assert_eq!(unbilled.paid, U256::ZERO, "nothing may leave the sender");
        assert_eq!(unbilled.received(), U256::ZERO, "no fee may reach any recipient");
        assert_eq!(unbilled.nonce(sender), 1, "the protocol nonce is still consumed");
        let slot = unbilled.state.get(&target).and_then(|acc| acc.storage.get(&U256::ZERO));
        assert_eq!(slot.map(|s| s.present_value), Some(U256::ZERO), "slot 0 is restored");

        let billed = settle(BaseUpgrade::Everest, &signed, sender, &contracts);
        assert!(billed.success);
        assert_eq!(billed.gas_used, gas_limit, "fees on: the reserved budget is billed");
    }

    /// With fee charging disabled nothing is charged, so a payer who cannot afford
    /// the worst-case fee is not rejected: the standard handler skips
    /// `ensure_enough_balance` under the flag, and so must the EIP-8130 path. With
    /// fees on the same transaction is rejected as unable to pay.
    #[test]
    fn fee_charge_disabled_execution_skips_the_payer_balance_check() {
        let key = signing_key(0x7e);
        let sender = eoa_address(&key);
        let target = address!("0x00000000000000000000000000000000000000cc");
        let contracts = [(target, bytes!("00"))];
        let value = U256::from(5);

        let tx = TxEip8130 {
            calls: vec![vec![Call { to: target, value, data: Bytes::new() }]],
            ..base_tx()
        };
        let signed = eoa_signed(tx, &key);
        let worst_case_fee =
            U256::from(signed.tx().gas_limit) * U256::from(signed.tx().max_fee_per_gas);
        assert!(value < worst_case_fee, "fixture must not afford the worst-case fee");

        // The sender holds only the call's `value`.
        let evm = |disable_fee_charge: bool| {
            let mut evm = evm_with_accounts(value, sender, &contracts);
            evm.ctx_mut().cfg.disable_fee_charge = disable_fee_charge;
            evm
        };

        let err = evm(false).transact_raw(into_base_tx(&signed)).unwrap_err();
        assert!(
            matches!(
                err,
                EVMError::Transaction(BaseTransactionError::Base(
                    InvalidTransaction::LackOfFundForMaxFee { .. }
                ))
            ),
            "fees on: an underfunded payer is rejected, got {err:?}"
        );

        let metered = metered_call(&mut evm(true), &signed, sender).tx_gas_used();
        let mut fee_free = evm(true);
        let outcome = fee_free.transact_raw(into_base_tx(&signed)).expect("fee-free execution");
        assert!(outcome.result.is_success(), "expected success, got {:?}", outcome.result);
        assert_eq!(outcome.result.tx_gas_used(), metered, "must report metered gas");
        let balance = |address: Address| {
            outcome.state.get(&address).map(|acc| acc.info.balance).unwrap_or_default()
        };
        assert_eq!(balance(sender), U256::ZERO, "only the call's value leaves the sender");
        assert_eq!(balance(target), value, "the call's value is delivered");
        assert_eq!(balance(BENEFICIARY), U256::ZERO, "no fee reaches the beneficiary");
        assert_eq!(balance(Predeploys::BASE_FEE_VAULT), U256::ZERO);
        assert_eq!(outcome.state.get(&sender).map(|acc| acc.info.nonce), Some(1));
    }

    /// Builds an EVM with `balance` funded to `sender`, deploying `code` at
    /// each contract address and pre-seeding the given `(address, slot, value)`
    /// storage entries. Used by estimate tests that need known storage state.
    fn evm_with_accounts_and_storage(
        balance: U256,
        sender: Address,
        contracts: &[(Address, Bytes)],
        storage: &[(Address, U256, U256)],
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
        for &(addr, slot, value) in storage {
            db.insert_account_storage(addr, slot, value).unwrap();
        }
        Context::base()
            .with_db(db)
            .with_cfg(
                CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Everest))
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
    fn simulate_estimate_covers_gross_gas_when_sstore_refund_earned() {
        // Proof that the estimate returns the GROSS call spend, not the net
        // billable charge. The target contract clears storage slot 0 (pre-seeded
        // to 1), earning an EIP-3529 SSTORE_CLEARS refund (~4800 gas). The net
        // charge `charge_gas` (gross less the refund, raised to the EIP-7623 floor
        // so the limit is admissible) as gas_limit leaves the call pool one
        // refund-unit short; the refund swamps any calldata-encoding
        // variance between the two gas-limit values (~16 gas), so that run
        // reliably OOGs. Assertions:
        //   - estimate_gas >= the metered gross charge, and > charge_gas
        //   - executing at gas_limit = charge_gas reverts (pool too small)
        //   - executing at gas_limit = estimate_gas succeeds
        //
        // Target bytecode: PUSH1 0, PUSH1 0, SSTORE (slot 0 ← 0), STOP.
        let key = signing_key(0x7b);
        let sender = eoa_address(&key);
        let target = address!("0x00000000000000000000000000000000000000e1");
        // PUSH1 0, PUSH1 0, SSTORE, STOP
        let sstore_clears = bytes!("600060005500");
        let initial = U256::from(10u64).pow(U256::from(18u64));
        // Slot 0 starts non-zero so the SSTORE (slot 0 ← 0) earns a refund.
        let storage = [(target, U256::ZERO, U256::from(1u64))];

        // EIP-3529 refund for clearing a non-zero slot.
        const SSTORE_CLEARS_REFUND: u64 = 4_800;

        // --- reference run at a generous limit to obtain the gross charge ---
        let mut tx_ref = base_tx();
        tx_ref.calls = vec![vec![Call { to: target, value: U256::ZERO, data: Bytes::new() }]];
        let signed_ref = eoa_signed(tx_ref, &key);
        let mut evm_ref = evm_with_accounts_and_storage(
            initial,
            sender,
            &[(target, sstore_clears.clone())],
            &storage,
        );
        let gross_gas = metered_call(&mut evm_ref, &signed_ref, sender).tx_gas_used();
        let sender_floor =
            IntrinsicGas::compute(&signed_ref, sender).expect("intrinsic gas").sender_floor();
        let charge_gas = (gross_gas - SSTORE_CLEARS_REFUND).max(sender_floor);

        // --- estimate (simulation never commits state) ---
        let mut tx_sim = base_tx();
        tx_sim.calls = vec![vec![Call { to: target, value: U256::ZERO, data: Bytes::new() }]];
        let signed_sim = eoa_signed(tx_sim, &key);
        let mut evm_sim = evm_with_accounts_and_storage(
            initial,
            sender,
            &[(target, sstore_clears.clone())],
            &storage,
        );
        evm_sim.ctx_mut().tx = into_base_tx(&signed_sim);
        evm_sim.ctx_mut().tx.base.caller = sender;
        let estimate_gas = Eip8130Executor::simulate(&mut evm_sim)
            .expect("estimation should succeed")
            .tx_gas_used();

        // The estimate covers the gross charge, so it strictly exceeds the net
        // charge because a refund was earned.
        assert!(estimate_gas >= gross_gas, "estimate must cover the gross charge");
        assert!(
            estimate_gas > charge_gas,
            "estimate_gas ({estimate_gas}) must exceed net charge ({charge_gas}): \
             refund must not reduce the gas limit"
        );

        // --- execute at gas_limit = charge_gas → must revert (OOG: refund was
        //     subtracted from pool but is not available during execution) ---
        let mut tx_low = base_tx();
        tx_low.gas_limit = charge_gas;
        tx_low.calls = vec![vec![Call { to: target, value: U256::ZERO, data: Bytes::new() }]];
        let signed_low = eoa_signed(tx_low, &key);
        let mut evm_low = evm_with_accounts_and_storage(
            initial,
            sender,
            &[(target, sstore_clears.clone())],
            &storage,
        );
        let result_low =
            evm_low.transact_raw(into_base_tx(&signed_low)).expect("tx should not error");
        assert!(
            matches!(result_low.result, ExecutionResult::Revert { .. }),
            "execution at gas_limit = charge_gas must revert: call pool too small \
             because net charge subtracted the refund"
        );

        // --- execute at gas_limit = estimate_gas → must succeed ---
        let mut tx_ok = base_tx();
        tx_ok.gas_limit = estimate_gas;
        tx_ok.calls = vec![vec![Call { to: target, value: U256::ZERO, data: Bytes::new() }]];
        let signed_ok = eoa_signed(tx_ok, &key);
        let mut evm_ok =
            evm_with_accounts_and_storage(initial, sender, &[(target, sstore_clears)], &storage);
        let result_ok = evm_ok.transact_raw(into_base_tx(&signed_ok)).expect("tx should not error");
        assert!(
            result_ok.result.is_success(),
            "execution at gas_limit = estimate_gas must succeed"
        );
    }

    #[test]
    fn simulate_estimate_covers_63_64_gas_retention_in_nested_calls() {
        // Proof that the estimate accounts for EIP-150's 63/64 gas forwarding
        // rule in nested calls. A forwarder contract calls a gas-sink contract
        // using `GAS` (all remaining gas); the sink does 100 cold SLOADs of
        // distinct slots (~210 000 gas). With a naive gas limit equal to the
        // metered charge, the 1/64 retained by the forwarder at each hop
        // leaves the sink short; the binary search finds the minimum feasible
        // pool and returns a strictly larger estimate. Assertions:
        //   - estimate_gas > charge_gas (search exceeded raw charge)
        //   - executing at gas_limit = charge_gas reverts (sink OOGs, forwarder
        //     reverts because it checks the CALL return value)
        //   - executing at gas_limit = estimate_gas succeeds
        //
        // Sink bytecode (0xe3): PUSH1 100 counter, loop: DUP1 (slot = counter),
        //   SLOAD, POP, PUSH1 1, SWAP1, SUB, DUP1, PUSH1 2 (JUMPDEST), JUMPI,
        //   POP, STOP.
        // Forwarder bytecode (0xe2): 5×PUSH1 0, PUSH20 sink, GAS, CALL, ISZERO,
        //   PUSH1 <revert_label>, JUMPI, STOP, JUMPDEST, PUSH1 0, PUSH1 0, REVERT.
        //   Reverts when the inner CALL fails, propagating sink OOG to the phase.
        let key = signing_key(0x7c);
        let sender = eoa_address(&key);
        let forwarder = address!("0x00000000000000000000000000000000000000e2");
        let sink = address!("0x00000000000000000000000000000000000000e3");

        // Sink: PUSH1 100, JUMPDEST@2, DUP1, SLOAD (cold per-slot), POP, PUSH1 1,
        //       SWAP1, SUB, DUP1, PUSH1 2, JUMPI, POP, STOP
        let sink_code = bytes!("60645b80545060019003806002575000");
        // Forwarder: PUSH1 0 ×5, PUSH20 sink (0xe3), GAS, CALL, ISZERO,
        //            PUSH1 0x26 (=38), JUMPI, STOP, JUMPDEST@38, PUSH1 0, PUSH1 0, REVERT
        // Offset check: 5×2=10, PUSH20=1+20=21 → ends @31, GAS@31, CALL@32,
        //               ISZERO@33, PUSH1 0x26 @34, JUMPI@36, STOP@37,
        //               JUMPDEST@38 ← matches 0x26 ✓
        let mut fwd_bytes = Vec::new();
        fwd_bytes.extend_from_slice(bytes!("6000600060006000600073").as_ref());
        fwd_bytes.extend_from_slice(sink.as_slice());
        fwd_bytes.extend_from_slice(bytes!("5af115602657005b60006000fd").as_ref());
        let fwd_code = Bytes::from(fwd_bytes);

        let initial = U256::from(10u64).pow(U256::from(18u64));

        // --- reference run at a generous limit for the metered charge ---
        let mut tx_ref = base_tx();
        tx_ref.calls = vec![vec![Call { to: forwarder, value: U256::ZERO, data: Bytes::new() }]];
        let signed_ref = eoa_signed(tx_ref, &key);
        let mut evm_ref = evm_with_accounts(
            initial,
            sender,
            &[(forwarder, fwd_code.clone()), (sink, sink_code.clone())],
        );
        let reference = metered_call(&mut evm_ref, &signed_ref, sender);
        assert!(reference.is_success(), "reference run should succeed");
        let charge_gas = reference.tx_gas_used();

        // --- estimate (binary search must go beyond ceiling_spent) ---
        let mut tx_sim = base_tx();
        tx_sim.calls = vec![vec![Call { to: forwarder, value: U256::ZERO, data: Bytes::new() }]];
        let signed_sim = eoa_signed(tx_sim, &key);
        let mut evm_sim = evm_with_accounts(
            initial,
            sender,
            &[(forwarder, fwd_code.clone()), (sink, sink_code.clone())],
        );
        evm_sim.ctx_mut().tx = into_base_tx(&signed_sim);
        evm_sim.ctx_mut().tx.base.caller = sender;
        let estimate_gas = Eip8130Executor::simulate(&mut evm_sim)
            .expect("estimation should succeed")
            .tx_gas_used();

        // The search must find a strictly larger limit to cover the 63/64 loss.
        assert!(
            estimate_gas > charge_gas,
            "estimate_gas ({estimate_gas}) must exceed the metered charge ({charge_gas}): \
             the 63/64 retention requires a higher gas limit than the raw spend"
        );

        // --- execute at gas_limit = charge_gas → must revert (sink OOGs) ---
        let mut tx_low = base_tx();
        tx_low.gas_limit = charge_gas;
        tx_low.calls = vec![vec![Call { to: forwarder, value: U256::ZERO, data: Bytes::new() }]];
        let signed_low = eoa_signed(tx_low, &key);
        let mut evm_low = evm_with_accounts(
            initial,
            sender,
            &[(forwarder, fwd_code.clone()), (sink, sink_code.clone())],
        );
        let result_low =
            evm_low.transact_raw(into_base_tx(&signed_low)).expect("tx should not error");
        assert!(
            matches!(result_low.result, ExecutionResult::Revert { .. }),
            "execution at gas_limit = charge_gas must revert: sink OOGs under 63/64 forwarding"
        );

        // --- execute at gas_limit = estimate_gas → must succeed ---
        let mut tx_ok = base_tx();
        tx_ok.gas_limit = estimate_gas;
        tx_ok.calls = vec![vec![Call { to: forwarder, value: U256::ZERO, data: Bytes::new() }]];
        let signed_ok = eoa_signed(tx_ok, &key);
        let mut evm_ok =
            evm_with_accounts(initial, sender, &[(forwarder, fwd_code), (sink, sink_code)]);
        let result_ok = evm_ok.transact_raw(into_base_tx(&signed_ok)).expect("tx should not error");
        assert!(
            result_ok.result.is_success(),
            "execution at gas_limit = estimate_gas must succeed"
        );
    }

    #[test]
    fn simulate_supports_configured_account_path() {
        // The configured-sender path is estimable: simulation resolves the owner
        // self-actor from committed state and prices the declared authenticator
        // (here k1, via the prefixed `sender_auth` blob) without verifying the
        // signature. Previously this path was rejected as unsupported.
        let key = signing_key(0x78);
        let sender = eoa_address(&key);

        let mut tx = base_tx();
        tx.sender = Some(sender);
        let signed = configured_signed(tx, &key);

        let mut evm = evm_with(U256::from(10u64).pow(U256::from(18u64)), sender);
        evm.ctx_mut().tx = into_base_tx(&signed);
        evm.ctx_mut().tx.base.caller = sender;

        let result = Eip8130Executor::simulate(&mut evm)
            .expect("configured-account estimation should succeed");
        assert!(result.is_success(), "estimation should report success");
        assert!(result.tx_gas_used() > 0, "estimated gas should be positive");
    }

    sol! {
        struct ActorConfigAbi {
            address authenticator;
            uint48 expiry;
            uint16 scope;
        }
    }

    /// ABI-encodes `abi.encode(bytes32 actorId, ActorConfig, bytes policyData)`
    /// for an `AuthorizeActor` op payload (mirrors `AccountChangeApplier`'s
    /// decode shape).
    fn authorize_change_data(
        actor_id: B256,
        authenticator: Address,
        scope: u16,
        expiry: u64,
        policy_data: &[u8],
    ) -> Bytes {
        let abi = ActorConfigAbi {
            authenticator,
            expiry: alloy_primitives::aliases::U48::from(expiry),
            scope,
        };
        Bytes::from((actor_id, abi, Bytes::copy_from_slice(policy_data)).abi_encode_params())
    }

    #[test]
    fn simulate_sender_actor_id_hint_resolves_policy_after_account_changes() {
        // Without a hint, simulate publishes the account's self-actor. Gate that
        // self to `wrong` and authorize a session actor (gated to `allowed`) in
        // the same estimate's accountChanges: a call to `allowed` then reverts
        // under the self-actor, and succeeds only when `senderActorId` names the
        // session actor — proving the hint changes policy resolution post-apply.
        let owner = signing_key(0xa1);
        let account = eoa_address(&owner);
        let session = signing_key(0xa2);
        let session_addr = eoa_address(&session);
        let session_actor = AccountConfigurationStorage::self_actor_id(session_addr);
        let allowed = address!("0x00000000000000000000000000000000000000d1");
        let wrong = address!("0x00000000000000000000000000000000000000d2");
        let commitment = B256::repeat_byte(0x42);

        let mut policy_data = Vec::with_capacity(52);
        policy_data.extend_from_slice(allowed.as_slice());
        policy_data.extend_from_slice(commitment.as_slice());

        let mut tx = base_tx();
        tx.sender = Some(account);
        tx.account_changes = vec![AccountChange::ConfigChange(SignedAccountChanges {
            channel: AccountChangeChannel::Local,
            sequence: 0,
            changes: vec![SignedChange {
                change_type: ChangeType::AuthorizeActor,
                payload: authorize_change_data(
                    session_actor,
                    Eip8130Constants::K1_AUTHENTICATOR,
                    Eip8130Constants::SCOPE_POLICY,
                    0,
                    &policy_data,
                ),
            }],
            // Simulate's apply path does not verify config auth.
            signature: Bytes::new(),
        })];
        tx.calls = vec![vec![Call { to: allowed, value: U256::ZERO, data: Bytes::new() }]];
        let signed = configured_signed(tx, &owner);

        let mut evm = with_zenith(evm_with_accounts(
            U256::from(10u64).pow(U256::from(18u64)),
            account,
            &[(allowed, bytes!("00")), (wrong, bytes!("00"))],
        ));
        // Gate the account's self-actor away from `allowed` so the no-hint path
        // hits the node policy gate.
        seed_gated_sender(&mut evm, account, account, wrong);

        // No hint → self-actor (gated to `wrong`) → ActorPolicyViolation.
        {
            let mut tx = into_base_tx(&signed);
            tx.base.caller = account;
            if let Some(parts) = tx.eip8130.as_mut() {
                parts.mode = Eip8130ExecutionMode::Simulate;
                parts.simulation_sender_actor_id = None;
            }
            evm.ctx_mut().tx = tx;
            let result = Eip8130Executor::simulate(&mut evm).expect("simulate should not error");
            let ExecutionResult::Revert { output, .. } = &result else {
                panic!("expected policy-gate revert without hint, got {result:?}");
            };
            let expected = keccak256(b"ActorPolicyViolation(bytes32,address)");
            assert_eq!(&output[..4], &expected[..4]);
        }

        // Hint → session actor (gated to `allowed`, authorized in accountChanges).
        {
            let mut tx = into_base_tx(&signed);
            tx.base.caller = account;
            if let Some(parts) = tx.eip8130.as_mut() {
                parts.mode = Eip8130ExecutionMode::Simulate;
                parts.simulation_sender_actor_id = Some(session_actor);
            }
            evm.ctx_mut().tx = tx;
            let result = Eip8130Executor::simulate(&mut evm).expect("simulate should not error");
            assert!(
                result.is_success(),
                "hinted session actor should pass the policy gate, got {result:?}"
            );
        }
    }

    /// In open payer mode the account recovered from `payer_auth` pays gas, so
    /// an unfunded sender's transaction executes against the payer's balance.
    #[test]
    fn open_payer_is_charged_instead_of_the_sender() {
        let key = signing_key(0x34);
        let sender = eoa_address(&key);
        let payer_key = signing_key(0x35);
        let payer = eoa_address(&payer_key);

        let tx = TxEip8130 { payer: Some(Eip8130Constants::OPEN_PAYER), ..base_tx() };
        let sender_auth = eoa_sig(&key, tx.sender_signature_hash());
        let payer_auth = eoa_sig(&payer_key, tx.payer_signature_hash(sender));
        let signed = Eip8130Signed::new(tx, sender_auth, payer_auth);

        let initial = U256::from(10u64).pow(U256::from(18u64));
        let mut evm = evm_with(initial, payer);
        let outcome = evm.transact_raw(into_base_tx(&signed)).expect("open-payer tx executes");

        assert!(outcome.result.is_success(), "expected success, got {:?}", outcome.result);
        assert!(outcome.state[&payer].info.balance < initial, "payer must be charged");
        assert!(outcome.state[&sender].info.balance.is_zero(), "sender must not be charged");
    }

    /// A k1 payer's signature bytes are covered by its authentication cost, so
    /// a sponsored transaction whose execution clears the EIP-7623 floor meters
    /// exactly the k1 authentication on top of the sender's gas, and gas-limit
    /// billing settles the declared limit plus that same authentication.
    #[test]
    fn sponsored_k1_payer_settles_at_the_k1_authentication_cost() {
        let key = signing_key(0x36);
        let sender = eoa_address(&key);
        let payer_key = signing_key(0x37);
        let payer = eoa_address(&payer_key);
        let loader = address!("0x00000000000000000000000000000000000000c9");

        let tx = TxEip8130 {
            payer: Some(Eip8130Constants::OPEN_PAYER),
            calls: vec![vec![Call { to: loader, value: U256::ZERO, data: Bytes::new() }]],
            ..base_tx()
        };
        let sender_auth = eoa_sig(&key, tx.sender_signature_hash());
        let payer_auth = eoa_sig(&payer_key, tx.payer_signature_hash(sender));
        let signed = Eip8130Signed::new(tx, sender_auth, payer_auth);
        let intrinsic = IntrinsicGas::compute(&signed, sender).expect("intrinsic gas");
        assert_eq!(
            intrinsic.payer_auth_cost,
            Eip8130GasSchedule::AUTH_EXEC_K1 + Eip8130GasSchedule::COLD_SLOAD
        );

        let initial = U256::from(10u64).pow(U256::from(18u64));
        let fresh = || evm_with_accounts(initial, payer, &[(loader, warmth_probe_loader_code())]);
        let outcome = fresh().transact_raw(into_base_tx(&signed)).expect("sponsored tx executes");
        assert!(outcome.result.is_success(), "expected success, got {:?}", outcome.result);
        assert_eq!(outcome.result.tx_gas_used(), signed.tx().gas_limit + intrinsic.payer_auth_cost);

        // The probe's access is intrinsic (the first call); it runs its
        // padding, PUSH1 (3) and a cold SLOAD (2_100).
        let call_gas = WARMTH_PROBE_PAD_GAS + 3 + Eip8130GasSchedule::COLD_SLOAD;
        let sender_gas = intrinsic.sender_intrinsic() + call_gas;
        assert!(sender_gas > intrinsic.sender_floor(), "execution must clear the floor");
        let metered = metered_call(&mut fresh(), &signed, sender);
        assert!(metered.is_success(), "expected success, got {metered:?}");
        assert_eq!(metered.tx_gas_used(), sender_gas + intrinsic.payer_auth_cost);
    }

    /// The sender estimates an open payer transaction before any payer has
    /// signed: a `payer_auth` that does not recover still simulates, with the
    /// payer unknown rather than the sender.
    #[test]
    fn simulate_prices_an_open_payer_that_has_not_signed() {
        let key = signing_key(0x36);
        let sender = eoa_address(&key);
        let tx = TxEip8130 { payer: Some(Eip8130Constants::OPEN_PAYER), ..base_tx() };
        let sender_auth = eoa_sig(&key, tx.sender_signature_hash());
        // `v = 0xff` is never a valid recovery byte.
        let signed = Eip8130Signed::new(tx, sender_auth, Bytes::from(vec![0xffu8; 65]));
        let mut evm = evm_with(U256::from(10u64).pow(U256::from(18u64)), sender);
        evm.ctx_mut().tx = into_base_tx(&signed);
        evm.ctx_mut().tx.base.caller = sender;
        if let Some(parts) = evm.ctx_mut().tx.eip8130.as_mut() {
            parts.mode = Eip8130ExecutionMode::Simulate;
        }

        let result = Eip8130Executor::simulate(&mut evm).expect("an unsigned open payer simulates");
        assert!(result.is_success(), "expected success, got {result:?}");
    }

    #[test]
    fn underfunded_payer_is_rejected() {
        let key = signing_key(0x33);
        let sender = eoa_address(&key);
        let signed = eoa_signed(base_tx(), &key);

        // Far below the worst-case charge (gas_limit · max_fee_per_gas).
        let mut evm = evm_with(U256::from(1_000u64), sender);
        let err = evm.transact_raw(into_base_tx(&signed)).unwrap_err();
        assert!(
            matches!(
                err,
                EVMError::Transaction(BaseTransactionError::Base(
                    InvalidTransaction::LackOfFundForMaxFee { .. }
                ))
            ),
            "got {err:?}"
        );
        // An invalid-transaction error, so the builder skips the transaction
        // instead of aborting the payload.
        assert!(err.as_invalid_tx_err().is_some());
    }

    #[test]
    fn single_phase_call_executes_against_contract_code() {
        // Contract that simply STOPs: `0x00`.
        let target = address!("0x00000000000000000000000000000000000000c1");
        let key = signing_key(0x44);
        let sender = eoa_address(&key);

        let mut tx = base_tx();
        tx.calls = vec![vec![Call { to: target, value: U256::ZERO, data: Bytes::new() }]];
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
        tx.calls = vec![vec![Call { to: target, value: U256::ZERO, data: Bytes::new() }]];
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

    /// Database error returned by [`StorageFailDb`] when a gated address is read.
    #[derive(Debug)]
    struct StorageUnavailable;

    impl core::fmt::Display for StorageUnavailable {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("storage temporarily unavailable")
        }
    }

    impl core::error::Error for StorageUnavailable {}

    impl DBErrorMarker for StorageUnavailable {}

    /// Wraps an [`InMemoryDB`] and fails every `storage` read for a single
    /// address, modelling a node-local backend failure (e.g. a missing trie
    /// node) encountered while an EVM call executes an `SLOAD`. All other reads
    /// delegate to the inner database.
    #[derive(Debug)]
    struct StorageFailDb {
        inner: InMemoryDB,
        fail_storage_at: Address,
    }

    impl Database for StorageFailDb {
        type Error = StorageUnavailable;

        fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
            Ok(self.inner.basic(address).expect("infallible"))
        }

        fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
            Ok(self.inner.code_by_hash(code_hash).expect("infallible"))
        }

        fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
            if address == self.fail_storage_at {
                return Err(StorageUnavailable);
            }
            Ok(self.inner.storage(address, index).expect("infallible"))
        }

        fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
            Ok(self.inner.block_hash(number).expect("infallible"))
        }
    }

    #[test]
    fn db_failure_during_call_propagates_as_error_not_revert() {
        // A node-local database failure raised *inside* a call's execution (here
        // an `SLOAD` the backend cannot serve) must abort the whole transaction
        // as a fatal `EVMError::Database`, never be folded into the deterministic
        // "phase reverted" path — otherwise a node that hits the failure would
        // include the tx as reverted while a healthy node would execute it,
        // forking consensus.
        let target = address!("0x00000000000000000000000000000000000000c7");
        let key = signing_key(0x77);
        let sender = eoa_address(&key);

        let mut tx = base_tx();
        tx.calls = vec![vec![Call { to: target, value: U256::ZERO, data: Bytes::new() }]];
        let signed = eoa_signed(tx, &key);

        // Target code: PUSH1 0x00, SLOAD, STOP. The `SLOAD` forces a storage read
        // of `target`, which the wrapping database refuses to serve.
        let code = bytes!("60005400");
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            sender,
            AccountInfo { balance: U256::from(10u64).pow(U256::from(18u64)), ..Default::default() },
        );
        db.insert_account_info(
            target,
            AccountInfo {
                code_hash: keccak256(&code),
                code: Some(Bytecode::new_raw(code)),
                ..Default::default()
            },
        );
        let db = StorageFailDb { inner: db, fail_storage_at: target };

        let mut evm = Context::base()
            .with_db(db)
            .with_cfg(
                CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Everest))
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

        let err = evm.transact_raw(into_base_tx(&signed)).unwrap_err();
        assert!(
            matches!(err, EVMError::Database(_)),
            "DB failure during a call must surface as a fatal database error, got {err:?}",
        );
    }

    #[test]
    fn db_failure_during_pre_call_is_fatal_not_an_invalid_transaction() {
        // The pre-call pipeline reads the nonce manager through the journal
        // storage provider. A backend failure there must abort as a fatal error:
        // an invalid-transaction error would let a node with a transient storage
        // fault reject a valid block.
        const NONCE_MANAGER: Address = address!("0x813000000000000000000000000000000000aa01");
        let key = signing_key(0x78);
        let sender = eoa_address(&key);
        let mut tx = base_tx();
        tx.nonce_key = U256::from(7);
        let signed = eoa_signed(tx, &key);

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            sender,
            AccountInfo { balance: U256::from(10u64).pow(U256::from(18u64)), ..Default::default() },
        );
        let db = StorageFailDb { inner: db, fail_storage_at: NONCE_MANAGER };

        let mut evm = Context::base()
            .with_db(db)
            .with_cfg(
                CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Everest))
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

        let err = evm.transact_raw(into_base_tx(&signed)).unwrap_err();
        assert!(err.is_fatal(), "pre-call DB failure must be fatal, got {err:?}");
        assert!(!err.is_invalid_tx_err(), "pre-call DB failure is not an invalid tx: {err:?}");
    }

    #[test]
    fn call_warms_coinbase_per_eip3651() {
        // EIP-3651: a dispatched call must see the coinbase pre-warmed, exactly
        // as a call in a normal transaction does. `BALANCE(coinbase)` therefore
        // costs warm access (100) rather than cold (2600). We compare it against
        // an otherwise-identical `BALANCE` of an untouched address (cold) and
        // require the coinbase read to be strictly cheaper; without coinbase
        // warming both reads are cold and the gas is identical.
        fn balance_of(addr: Address) -> Bytes {
            // PUSH20 <addr>, BALANCE, STOP
            let mut code = Vec::with_capacity(23);
            code.push(0x73);
            code.extend_from_slice(addr.as_slice());
            code.push(0x31);
            code.push(0x00);
            Bytes::from(code)
        }

        let target = address!("0x00000000000000000000000000000000000000c8");
        let cold_account = address!("0x00000000000000000000000000000000000000dd");

        let run = |code: Bytes, signer: u8| -> u64 {
            let key = signing_key(signer);
            let sender = eoa_address(&key);
            let mut tx = base_tx();
            tx.calls = vec![vec![Call { to: target, value: U256::ZERO, data: Bytes::new() }]];
            let signed = eoa_signed(tx, &key);
            let mut evm = evm_with_accounts(
                U256::from(10u64).pow(U256::from(18u64)),
                sender,
                &[(target, code)],
            );
            let result = metered_call(&mut evm, &signed, sender);
            assert!(result.is_success(), "expected success, got {result:?}");
            result.tx_gas_used()
        };

        // Both calls are byte-for-byte identical except the address read, so the
        // only gas difference is the cold-vs-warm access cost of that address.
        let warm_coinbase_gas = run(balance_of(BENEFICIARY), 0x78);
        let cold_account_gas = run(balance_of(cold_account), 0x79);

        assert!(
            warm_coinbase_gas < cold_account_gas,
            "BALANCE(coinbase) ({warm_coinbase_gas}) must be cheaper than a cold \
             BALANCE ({cold_account_gas}) because EIP-3651 pre-warms the coinbase",
        );
    }

    #[test]
    fn warmth_does_not_leak_across_transactions() {
        let key = signing_key(0x66);
        let sender = eoa_address(&key);

        let loader = address!("0x00000000000000000000000000000000000000c8");
        // JUMPDEST padding (see `warmth_probe_loader_code`) then PUSH1 0, SLOAD,
        // STOP: the padding keeps execution above the EIP-7623 floor so the
        // cold-vs-warm SLOAD delta below is not masked by the floor clamp.
        let loader_code = warmth_probe_loader_code();

        let mut evm = evm_with_accounts(
            U256::from(10u64).pow(U256::from(18u64)),
            sender,
            &[(loader, loader_code)],
        );

        let mut warm_tx = base_tx();
        warm_tx.calls = vec![vec![Call { to: loader, value: U256::ZERO, data: Bytes::new() }]];
        let warm_signed = eoa_signed(warm_tx, &key);
        let warm_outcome =
            evm.transact_raw(into_base_tx(&warm_signed)).expect("tx should be included");
        assert!(matches!(warm_outcome.result, ExecutionResult::Success { .. }));
        revm::DatabaseCommit::commit(evm.ctx_mut().journal_mut().db_mut(), warm_outcome.state);

        // Tx 1 already bumped the protocol nonce (0 -> 1) (committed above), so
        // tx 2 is a second-use transaction:
        // base AA_BASE_COST 12_000
        // payload EIP-2028 DA over account_changes, calls, metadata 220
        // nonce_key protocol nonce (key 0) 0
        // sender_auth ECRECOVER 3_000 + config SLOAD 2_100 5_100
        // first_call cold loader account (COLD_ACCOUNT_ACCESS_COST)
        // call JUMPDEST pad (WARMTH_PROBE_PAD_GAS) + PUSH1 (3) + COLD SLOAD (2_100) + STOP
        // total 19_423 + COLD_ACCOUNT_ACCESS_COST + WARMTH_PROBE_PAD_GAS
        let mut load_tx = base_tx();
        load_tx.nonce_sequence = 1;
        load_tx.calls = vec![vec![Call { to: loader, value: U256::ZERO, data: Bytes::new() }]];
        let load_signed = eoa_signed(load_tx, &key);
        let load_result = metered_call(&mut evm, &load_signed, sender);
        assert!(matches!(load_result, ExecutionResult::Success { .. }));

        assert_eq!(
            load_result.tx_gas_used(),
            19_423 + COLD_ACCOUNT_ACCESS_COST + WARMTH_PROBE_PAD_GAS,
            "loader SLOAD must be COLD (2_100); a warm read (100) would be 2_000 \
             less, meaning tx 1's warmth leaked across the transaction boundary",
        );
    }

    #[test]
    fn committed_phase_warms_later_phase() {
        // Intra-transaction warmth: a phase that SLOADs and COMMITS must leave
        // the slot warm for a later phase in the SAME tx, so the second SLOAD is
        // charged warm (100), not cold (2100).
        // This exercises `checkpoint_commit` merging a phase's warmth into the parent
        // journal, the flip side of the discard path, and reachable only WITHIN a tx before
        // `finalize`.
        let key = signing_key(0x66);
        let sender = eoa_address(&key);

        let loader = address!("0x00000000000000000000000000000000000000c8");
        // JUMPDEST padding (see `warmth_probe_loader_code`) then PUSH1 0, SLOAD,
        // STOP: the padding keeps each phase's execution above the EIP-7623 floor
        // so the intra-transaction cold-vs-warm SLOAD delta is not masked.
        let loader_code = warmth_probe_loader_code();

        let mut evm = evm_with_accounts(
            U256::from(10u64).pow(U256::from(18u64)),
            sender,
            &[(loader, loader_code)],
        );

        let mut tx = base_tx();
        tx.calls = vec![
            vec![Call { to: loader, value: U256::ZERO, data: Bytes::new() }],
            vec![Call { to: loader, value: U256::ZERO, data: Bytes::new() }],
        ];
        let signed = eoa_signed(tx, &key);
        let result = metered_call(&mut evm, &signed, sender);
        assert!(matches!(result, ExecutionResult::Success { .. }));

        // First-use, single-tx, two phases each calling `loader`:
        // base AA_BASE_COST 12_000
        // payload EIP-2028 DA over account_changes, calls, metadata 392
        // nonce_key protocol nonce (key 0), even on first use 0
        // sender_auth ECRECOVER 3_000 + config SLOAD 2_100 5_100
        // first_call cold loader account in phase 0 (COLD_ACCOUNT_ACCESS_COST)
        // access warm loader account in phase 1 (WARM_STORAGE_READ_COST)
        // call phase 0 JUMPDEST pad + PUSH1 (3) + COLD SLOAD (2_100) + STOP
        // call phase 1 JUMPDEST pad + PUSH1 (3) + WARM SLOAD (100) + STOP
        // total 19_698 + COLD_ACCOUNT_ACCESS_COST + WARM_STORAGE_READ_COST
        // + 2 * WARMTH_PROBE_PAD_GAS (the pad runs once per phase)
        assert_eq!(
            result.tx_gas_used(),
            19_698 + COLD_ACCOUNT_ACCESS_COST + WARM_STORAGE_READ_COST + 2 * WARMTH_PROBE_PAD_GAS,
            "phase 1's SLOAD must be WARM (100): committed phase 0 warmed \
             (loader, slot 0). A cold read (2_100) would be 2_000 more, meaning \
             the committed phase's warmth failed to carry across phases",
        );
    }

    #[test]
    fn validation_error_discards_warmth() {
        let key = signing_key(0x66);
        let sender = eoa_address(&key);

        let loader = address!("0x00000000000000000000000000000000000000c8");
        // JUMPDEST padding (see `warmth_probe_loader_code`) then PUSH1 0, SLOAD,
        // STOP: the padding keeps execution above the EIP-7623 floor so the
        // cold-vs-warm SLOAD delta below is not masked by the floor clamp.
        let loader_code = warmth_probe_loader_code();

        let mut evm = evm_with_accounts(
            U256::from(10u64).pow(U256::from(18u64)),
            sender,
            &[(loader, loader_code)],
        );

        let mut invalid_tx = base_tx();
        invalid_tx.nonce_sequence = 5;
        invalid_tx.calls =
            vec![vec![Call { to: loader, value: U256::ZERO, data: bytes!("60006000fd") }]];
        let invalid_signed = eoa_signed(invalid_tx, &key);
        let invalid_outcome = evm.transact_raw(into_base_tx(&invalid_signed)).unwrap_err();
        assert!(
            matches!(invalid_outcome, EVMError::Transaction(BaseTransactionError::Eip8130(_))),
            "validation error should surface as a validation error, got {invalid_outcome:?}",
        );

        let mut load_tx = base_tx();
        load_tx.calls = vec![vec![Call { to: loader, value: U256::ZERO, data: Bytes::new() }]];
        let load_signed = eoa_signed(load_tx, &key);
        let load_result = metered_call(&mut evm, &load_signed, sender);
        assert!(matches!(load_result, ExecutionResult::Success { .. }));

        // `load_tx` is a self-paying EOA transaction with one phase calling
        // `loader` (PUSH1 0, SLOAD, STOP). Its gas splits into the EIP-8130
        // sender-intrinsic charge (including the loader's cold access as the
        // first call) plus the dispatched call:
        // base AA_BASE_COST 12_000
        // payload EIP-2028 DA over account_changes, calls, metadata 220
        // nonce_key protocol nonce (key 0), even on first use 0
        // sender_auth ECRECOVER 3_000 + config SLOAD 2_100 5_100
        // first_call cold loader account (COLD_ACCOUNT_ACCESS_COST)
        // call JUMPDEST pad (WARMTH_PROBE_PAD_GAS) + PUSH1 (3) + COLD SLOAD (2_100) + STOP
        // total 19_423 + COLD_ACCOUNT_ACCESS_COST + WARMTH_PROBE_PAD_GAS
        assert_eq!(
            load_result.tx_gas_used(),
            19_423 + COLD_ACCOUNT_ACCESS_COST + WARMTH_PROBE_PAD_GAS,
            "loader SLOAD must be COLD (2_100); a warm read (100) would be 2_000 \
             less, meaning the discarded invalid tx leaked warmth",
        );
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
            vec![Call { to: reverter, value: U256::ZERO, data: Bytes::new() }],
            vec![Call { to: storer, value: U256::ZERO, data: Bytes::new() }],
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

    /// Canonical Solidity packing of an `ActorConfig` word (authenticator 0..160,
    /// expiry 160..208, scope 208..224).
    fn pack_actor(authenticator: Address, scope: u16, expiry: u64) -> U256 {
        U256::from_be_slice(authenticator.as_slice())
            | (U256::from(expiry) << 160)
            | (U256::from(scope) << 208)
    }

    /// Signs `tx` for a configured sender as `K1_AUTHENTICATOR || sig`.
    fn configured_signed(tx: TxEip8130, signer: &SigningKey) -> Eip8130Signed {
        let hash = tx.sender_signature_hash();
        let mut auth = Vec::with_capacity(85);
        auth.extend_from_slice(Eip8130Constants::K1_AUTHENTICATOR.as_slice());
        auth.extend_from_slice(&eoa_sig(signer, hash));
        Eip8130Signed::new(tx, Bytes::from(auth), Bytes::new())
    }

    /// Seeds a policy-gated k1 actor for `account`, authorized to the `signer`
    /// key and gated to `target`, then commits it. POLICY-only (plus payer/nonce
    /// grants): OPERATOR would override POLICY and leave the sender ungated.
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
                acc.actors
                    .at_mut(&actor_id)
                    .at_mut(&account)
                    .write(pack_actor(
                        Eip8130Constants::K1_AUTHENTICATOR,
                        Eip8130Constants::SCOPE_POLICY
                            | Eip8130Constants::SCOPE_SELF_PAYER
                            | Eip8130Constants::SCOPE_NONCE,
                        0,
                    ))
                    .unwrap();
                acc.set_policy(account, actor_id, target, B256::ZERO).unwrap();
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
        tx.calls = vec![vec![Call { to: forbidden, value: U256::ZERO, data: Bytes::new() }]];
        let signed = configured_signed(tx, &signer);

        let mut evm = with_zenith(evm_with(U256::from(10u64).pow(U256::from(18u64)), account));
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
        tx.calls = vec![vec![Call { to: allowed, value: U256::ZERO, data: Bytes::new() }]];
        let signed = configured_signed(tx, &signer);

        let mut evm = with_zenith(evm_with_accounts(
            U256::from(10u64).pow(U256::from(18u64)),
            account,
            &[(allowed, bytes!("00"))],
        ));
        seed_gated_sender(&mut evm, account, signer_addr, allowed);

        let outcome = evm.transact_raw(into_base_tx(&signed)).expect("tx should execute");
        assert!(outcome.result.is_success(), "expected success, got {:?}", outcome.result);
    }

    #[test]
    fn pre_zenith_ignores_keystore_actors() {
        // The Keystore authorizes `signer` for `account`, but before Zenith
        // there is no Keystore: a named sender must sign with its own key.
        let account = address!("0x00000000000000000000000000000000000000ca");
        let allowed = address!("0x00000000000000000000000000000000000000cb");
        let signer = signing_key(0x99);
        let signer_addr = eoa_address(&signer);

        let mut tx = base_tx();
        tx.sender = Some(account);
        tx.calls = vec![vec![Call { to: allowed, value: U256::ZERO, data: Bytes::new() }]];
        let signed = configured_signed(tx, &signer);

        let mut evm = evm_with(U256::from(10u64).pow(U256::from(18u64)), account);
        seed_gated_sender(&mut evm, account, signer_addr, allowed);

        let err = evm.transact_raw(into_base_tx(&signed)).unwrap_err();
        let EVMError::Transaction(BaseTransactionError::Eip8130(reason)) = err else {
            panic!("expected an Eip8130 validity rejection, got {err:?}");
        };
        assert!(reason.contains("does not recover to the named account"), "got {reason:?}");
    }

    /// Before Zenith, a non-secp256k1 authenticator and a `Create` are rejected
    /// as unsupported, in both execution and simulation.
    #[test]
    fn pre_zenith_rejects_unsupported_authenticator_and_account_change() {
        let account = address!("0x00000000000000000000000000000000000000ce");
        let mut tx = base_tx();
        tx.sender = Some(account);
        let mut auth = Eip8130Contracts::P256_AUTHENTICATOR.to_vec();
        auth.extend_from_slice(&[0u8; 128]);
        let p256 = Eip8130Signed::new(tx, Bytes::from(auth), Bytes::new());
        let (derived, create) =
            counterfactual_create_signed(&signing_key(0x9b), bytes!("00"), Vec::new());

        for (signed, sender, reason) in [
            (p256, account, "unsupported authenticator"),
            (create, derived, "unsupported account change type"),
        ] {
            let mut evm = evm_with(U256::from(10u64).pow(U256::from(18u64)), sender);
            let err = evm.transact_raw(into_base_tx(&signed)).unwrap_err();
            let EVMError::Transaction(BaseTransactionError::Eip8130(got)) = err else {
                panic!("expected an Eip8130 validity rejection, got {err:?}");
            };
            assert!(got.contains(reason), "got {got:?}, expected {reason:?}");

            let mut sim = into_base_tx(&signed);
            sim.base.caller = sender;
            if let Some(parts) = sim.eip8130.as_mut() {
                parts.mode = Eip8130ExecutionMode::Simulate;
            }
            evm.ctx_mut().tx = sim;
            let err = Eip8130Executor::simulate(&mut evm).unwrap_err();
            assert!(err.to_string().contains(reason), "got {err}, expected {reason:?}");
        }
    }

    /// EIP-8130 exists only from Everest: a block before it cannot include or
    /// simulate one, whatever path delivered the transaction.
    #[test]
    fn eip8130_is_rejected_before_everest() {
        let key = signing_key(0x9c);
        let sender = eoa_address(&key);
        let signed = eoa_signed(base_tx(), &key);
        let mut evm = evm_with(U256::from(10u64).pow(U256::from(18u64)), sender);
        evm.ctx_mut().cfg.spec = BaseSpecId::new(BaseUpgrade::Denim);

        let err = evm.transact_raw(into_base_tx(&signed)).unwrap_err();
        assert!(err.to_string().contains("not active before Everest"), "got {err}");

        let mut sim = into_base_tx(&signed);
        sim.base.caller = sender;
        if let Some(parts) = sim.eip8130.as_mut() {
            parts.mode = Eip8130ExecutionMode::Simulate;
        }
        evm.ctx_mut().tx = sim;
        let err = Eip8130Executor::simulate(&mut evm).unwrap_err();
        assert!(err.to_string().contains("not active before Everest"), "got {err}");
    }

    /// EIP-7825 bounds the gas an EIP-8130 transaction can consume.
    #[test]
    fn gas_above_the_per_transaction_cap_is_rejected() {
        let key = signing_key(0x9d);
        let sender = eoa_address(&key);
        let mut evm = evm_with(U256::MAX >> 1, sender);
        let cap = evm.ctx().cfg.tx_gas_limit_cap();
        let signed = eoa_signed(TxEip8130 { gas_limit: cap + 1, ..base_tx() }, &key);

        let err = evm.transact_raw(into_base_tx(&signed)).unwrap_err();
        assert!(err.to_string().contains("EIP-7825"), "got {err}");
    }

    /// Simulation never runs with more gas than a real transaction could carry:
    /// `gas_limit` plus the payer authentication ceiling stays within the
    /// EIP-7825 cap, so a sponsored estimate is always includable.
    #[test]
    fn simulation_gas_is_bounded_by_the_per_transaction_cap() {
        let key = signing_key(0x9f);
        let sender = eoa_address(&key);
        let payer = address!("0x00000000000000000000000000000000000000b7");
        // `JUMPDEST PUSH1 0 JUMP`: loops until it runs out of gas, so the
        // simulation consumes its whole gas ceiling.
        let looper = address!("0x00000000000000000000000000000000000000b8");
        let mut evm = evm_with_accounts(U256::MAX >> 1, sender, &[(looper, bytes!("5b600056"))]);
        let cap = evm.ctx().cfg.tx_gas_limit_cap();
        let tx = TxEip8130 {
            gas_limit: cap,
            payer: Some(payer),
            calls: vec![vec![Call { to: looper, value: U256::ZERO, data: Bytes::new() }]],
            ..base_tx()
        };
        let mut payer_auth = Eip8130Constants::K1_AUTHENTICATOR.to_vec();
        payer_auth.extend_from_slice(&[0xab; 65]);
        let signed = Eip8130Signed::new(tx, eoa_sig(&key, B256::ZERO), Bytes::from(payer_auth));
        let payer_auth_cost = IntrinsicGas::payer_auth_cost_of(&signed).unwrap();
        assert!(payer_auth_cost > 0);

        let mut sim = into_base_tx(&signed);
        sim.base.caller = sender;
        if let Some(parts) = sim.eip8130.as_mut() {
            parts.mode = Eip8130ExecutionMode::Simulate;
        }
        evm.ctx_mut().tx = sim;
        let result = Eip8130Executor::simulate(&mut evm).expect("simulation runs");
        assert!(!result.is_success(), "the loop exhausts the simulated gas");
        // The reported gas includes the payer authentication metered on top of
        // the simulated `gas_limit`.
        assert!(
            result.tx_gas_used() <= cap,
            "simulated gas {} (with payer authentication {payer_auth_cost}) exceeds the cap {cap}",
            result.tx_gas_used()
        );
    }

    /// A nonce-free transaction must carry `nonce_sequence == 0` at inclusion,
    /// not only at pool admission.
    #[test]
    fn nonce_free_with_nonzero_sequence_is_rejected_at_inclusion() {
        let key = signing_key(0x9e);
        let sender = eoa_address(&key);
        let tx = TxEip8130 {
            nonce_key: Eip8130Constants::NONCE_KEY_MAX,
            nonce_sequence: 1,
            valid_before: NOW * 1_000 + 20_000,
            ..base_tx()
        };
        let signed = eoa_signed(tx, &key);
        let mut evm = evm_with(U256::from(10u64).pow(U256::from(18u64)), sender);

        let err = evm.transact_raw(into_base_tx(&signed)).unwrap_err();
        assert!(err.to_string().contains("nonce-free transaction has nonce sequence"), "got {err}");
    }

    /// Builds a counterfactual-create [`Eip8130Signed`] for `key`'s owner whose
    /// derived CREATE2 address is the transaction sender, deploying `code` and
    /// dispatching `calls`. Returns the derived address alongside the signed tx.
    fn counterfactual_create_signed(
        key: &SigningKey,
        code: Bytes,
        calls: Vec<Vec<Call>>,
    ) -> (Address, Eip8130Signed) {
        let owner = eoa_address(key);
        let actor_id = {
            let mut id = [0u8; 32];
            id[12..].copy_from_slice(owner.as_slice());
            B256::from_slice(&id)
        };
        let initial_actors =
            vec![InitialActor::owner(actor_id, Eip8130Constants::K1_AUTHENTICATOR)];
        let create = CreateEntry {
            user_salt: B256::ZERO,
            code: code.clone(),
            initial_actors: initial_actors.clone(),
        };
        let derived =
            AccountChangeApplier::compute_address(create.user_salt, &code, &initial_actors)
                .expect("address derivation");

        let mut tx = base_tx();
        tx.sender = Some(derived);
        tx.account_changes = vec![AccountChange::Create(create)];
        tx.calls = calls;
        (derived, configured_signed(tx, key))
    }

    #[test]
    fn counterfactual_create_executes_and_is_included() {
        // End-to-end regression for the counterfactual smart-account CREATE bug
        // (PR #3766): a `0x79` create whose sender is the not-yet-existent CREATE2
        // address must authorize and be *included* through the full
        // `Eip8130Executor::execute` pipeline — not just the unit-level
        // `authorize_and_apply`. Before the fix this returned
        // `BaseTransactionError::Eip8130("...AuthenticatorMismatch")` and was rejected at every
        // block build. Non-empty runtime code mirrors the on-chain account.
        let key = signing_key(0xc1);
        let (derived, signed) = counterfactual_create_signed(&key, bytes!("00"), Vec::new());

        let initial_balance = U256::from(10u64).pow(U256::from(18u64));
        let mut evm = with_zenith(evm_with(initial_balance, derived));
        let outcome =
            evm.transact_raw(into_base_tx(&signed)).expect("counterfactual create should execute");

        assert!(outcome.result.is_success(), "expected success, got {:?}", outcome.result);
        // The create installed the account's runtime code and bumped its nonce.
        let created = outcome.state.get(&derived).expect("created account in state");
        assert_eq!(created.info.nonce, 1, "create sender nonce bumped");
        assert!(!created.info.is_empty_code_hash(), "created account has code");
        assert!(created.info.balance < initial_balance, "self-paid create charged");
    }

    /// Asserts a counterfactual create with `code` is rejected at inclusion with
    /// an `Eip8130` validity error whose reason contains `reason`, and that no
    /// balance is charged (the transaction is not included). Never panics — the
    /// point of the create-safety gate is that transaction-controlled runtimes
    /// surface as clean rejections rather than executor panics.
    fn assert_create_rejected(byte: u8, code: Bytes, reason: &str) {
        let key = signing_key(byte);
        let (derived, signed) = counterfactual_create_signed(&key, code, Vec::new());
        let mut evm = with_zenith(evm_with(U256::from(10u64).pow(U256::from(18u64)), derived));
        let err = evm.transact_raw(into_base_tx(&signed)).unwrap_err();
        let EVMError::Transaction(BaseTransactionError::Eip8130(got)) = err else {
            panic!("expected an Eip8130 validity rejection, got {err:?}");
        };
        assert!(got.contains(reason), "reason {got:?} does not contain {reason:?}");
    }

    #[test]
    fn create_rejects_oversized_runtime() {
        // EIP-170: a runtime larger than MAX_CODE_SIZE would be rejected by the
        // reference contract's CREATE2 deploy; the enshrined path must reject it
        // too rather than install oversized code at inclusion.
        let code = Bytes::from(vec![0x00u8; Eip8130Constants::MAX_CODE_SIZE + 1]);
        assert_create_rejected(0xb1, code, "MAX_CODE_SIZE");
    }

    #[test]
    fn create_rejects_leading_ef_runtime() {
        // EIP-3541: deployed code may not begin with 0xEF.
        assert_create_rejected(0xb2, bytes!("ef00"), "0xEF");
    }

    #[test]
    fn create_rejects_malformed_eip7702_runtime() {
        // The 3-byte 0xEF0100 prefix is a malformed EIP-7702 designator that
        // previously panicked `Bytecode::new_raw` (InvalidLength) when installed
        // as create runtime. It must now be a clean EIP-3541 rejection.
        assert_create_rejected(0xb3, bytes!("ef0100"), "0xEF");
    }

    #[test]
    fn create_rejects_full_eip7702_designator_runtime() {
        // A canonical 23-byte 0xEF0100||target designator must not be accepted as
        // create runtime (it would silently become an EIP-7702 delegation from a
        // Create-only transaction); EIP-3541 rejects it.
        let mut designator = vec![0xEF, 0x01, 0x00];
        designator.extend_from_slice(Address::repeat_byte(0x42).as_slice());
        assert_create_rejected(0xb4, Bytes::from(designator), "0xEF");
    }

    #[test]
    fn create_rejects_overwriting_existing_code() {
        // CREATE2 collision: the reference contract's deploy reverts when the
        // destination already holds code. The enshrined path installs runtime
        // directly, so it must reject a create whose derived address already has
        // preexisting (non-8130) bytecode instead of overwriting it.
        let key = signing_key(0xb5);
        let (derived, signed) = counterfactual_create_signed(&key, bytes!("6001"), Vec::new());
        let mut evm = with_zenith(evm_with(U256::from(10u64).pow(U256::from(18u64)), derived));
        seed_account_code(&mut evm, derived, Bytes::from_static(&[0xfe, 0xfe, 0xfe]));

        let err = evm.transact_raw(into_base_tx(&signed)).unwrap_err();
        let EVMError::Transaction(BaseTransactionError::Eip8130(got)) = err else {
            panic!("expected an Eip8130 validity rejection, got {err:?}");
        };
        assert!(got.contains("already has code"), "unexpected reason: {got}");
    }

    #[test]
    fn counterfactual_create_then_call_executes_and_is_included() {
        // The created account must be able to dispatch its `calls` in the same
        // transaction it is created in: the sender authenticates against the
        // freshly-installed unrestricted owner, then the calls run from the
        // created sender. Exercises the create-apply + call-dispatch path through
        // `Eip8130Executor::execute` end-to-end.
        let key = signing_key(0xc2);
        let target = address!("0x00000000000000000000000000000000000000ca");
        let (derived, signed) = counterfactual_create_signed(
            &key,
            bytes!("00"),
            vec![vec![Call { to: target, value: U256::ZERO, data: Bytes::new() }]],
        );

        let initial_balance = U256::from(10u64).pow(U256::from(18u64));
        let mut evm =
            with_zenith(evm_with_accounts(initial_balance, derived, &[(target, bytes!("00"))]));
        let outcome = evm
            .transact_raw(into_base_tx(&signed))
            .expect("counterfactual create + call should execute");
        assert!(outcome.result.is_success(), "expected success, got {:?}", outcome.result);
    }

    #[test]
    fn execution_window_normalizes_seconds_denominated_valid_before() {
        // The block timestamp is `NOW` seconds, so the consensus inclusion window
        // compares against `now_ms = NOW * 1000`. A `valid_before` supplied in
        // *seconds* must be normalized to milliseconds first: `NOW + 1` seconds
        // normalizes to `(NOW + 1) * 1000` ms, one second in the future, so the
        // transaction is includable. Without normalization the raw `NOW + 1`
        // would compare as far below `now_ms` and be wrongly rejected as expired.
        let key = signing_key(0x7a);
        let sender = eoa_address(&key);
        let target = address!("0x00000000000000000000000000000000000000d2");
        let initial = U256::from(10u64).pow(U256::from(18u64));

        let mut tx = base_tx();
        tx.calls = vec![vec![Call { to: target, value: U256::ZERO, data: Bytes::new() }]];
        tx.valid_before = NOW + 1;
        let signed = eoa_signed(tx, &key);
        let mut evm = evm_with_accounts(initial, sender, &[(target, bytes!("00"))]);
        let outcome = evm
            .transact_raw(into_base_tx(&signed))
            .expect("a seconds valid_before in the future must be admitted");
        assert!(outcome.result.is_success(), "expected success, got {:?}", outcome.result);

        // A seconds `valid_before` one second in the past normalizes to
        // `(NOW - 1) * 1000` ms and is strictly past `now_ms`, so the inclusion
        // window rejects it.
        let mut tx = base_tx();
        tx.calls = vec![vec![Call { to: target, value: U256::ZERO, data: Bytes::new() }]];
        tx.valid_before = NOW - 1;
        let signed = eoa_signed(tx, &key);
        let mut evm = evm_with_accounts(initial, sender, &[(target, bytes!("00"))]);
        let err = evm
            .transact_raw(into_base_tx(&signed))
            .expect_err("a seconds valid_before in the past must be rejected");
        let EVMError::Transaction(BaseTransactionError::Eip8130(got)) = err else {
            panic!("expected an Eip8130 validity rejection, got {err:?}");
        };
        assert!(got.contains("validity window has expired"), "unexpected reason: {got}");
    }

    #[test]
    fn execution_window_and_replay_ring_use_the_base_time_millisecond_clock() {
        // The block is `BLOCK_SECONDS` plus the 200 ms `BaseTime` part, so both
        // bounds and the nonce-free replay ring compare against `now_ms`, not
        // the whole second.
        const BLOCK_SECONDS: u64 = 1_700_000_000;
        const MILLIS_PART: u64 = 200;
        let now_ms = BLOCK_SECONDS * 1_000 + MILLIS_PART;
        let key = signing_key(0x7b);
        let sender = eoa_address(&key);
        let target = address!("0x00000000000000000000000000000000000000d3");
        let initial = U256::from(10u64).pow(U256::from(18u64));

        let transact = |nonce_free: bool, valid_after: u64, valid_before: u64| {
            let mut tx = base_tx();
            tx.calls = vec![vec![Call { to: target, value: U256::ZERO, data: Bytes::new() }]];
            if nonce_free {
                tx.nonce_key = Eip8130Constants::NONCE_KEY_MAX;
            }
            tx.valid_after = valid_after;
            tx.valid_before = valid_before;
            let signed = eoa_signed(tx, &key);
            let mut evm = evm_with_accounts_and_storage(
                initial,
                sender,
                &[(target, bytes!("00"))],
                &[(
                    Predeploys::BASE_TIME,
                    BaseTime::TIMESTAMP_MILLIS_PART_SLOT,
                    U256::from(MILLIS_PART),
                )],
            );
            evm.ctx_mut().block.timestamp = U256::from(BLOCK_SECONDS);
            evm.transact_raw(into_base_tx(&signed)).map(|outcome| outcome.result.is_success())
        };
        let rejection = |result: Result<bool, _>| match result {
            Err(EVMError::Transaction(BaseTransactionError::Eip8130(reason))) => reason,
            other => panic!("expected an Eip8130 rejection, got {other:?}"),
        };

        for nonce_free in [false, true] {
            let valid_before = now_ms + 20_000;
            assert_eq!(transact(nonce_free, now_ms, valid_before), Ok(true));
            assert!(
                rejection(transact(nonce_free, now_ms + 1, valid_before)).contains("not yet valid")
            );
            assert!(
                rejection(transact(nonce_free, 0, now_ms - 1))
                    .contains("validity window has expired")
            );
        }
        assert_eq!(transact(false, 0, now_ms), Ok(true));
        assert_eq!(
            rejection(transact(true, 0, now_ms)),
            "Revert",
            "the replay ring requires `valid_before` strictly after `now_ms`"
        );
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
            Call { to: target, value: U256::ZERO, data: Bytes::from(clear_data) },
            Call { to: target, value: U256::ZERO, data: Bytes::from(second_data) },
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
                CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Everest))
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

    /// Under gas-limit billing refunds no longer lower the bill: two transactions
    /// that differ only in their net SSTORE refund settle the same declared limit,
    /// while their storage effects stay distinct. How per-call refund deltas sum
    /// at the transaction level is pinned by
    /// `capped_refund_uses_signed_transaction_level_accounting`.
    #[test]
    fn cross_call_refunds_do_not_change_the_bill() {
        // Both transactions: call 1 clears slot 0 (+4800 clear refund). Call 2
        // either restores the original value (cancelling the clear refund) or
        // re-clears it (keeping the refund).
        let (restore_gas, restore_slot) = run_cross_call_refund_tx(0x99, 1);
        let (reclear_gas, reclear_slot) = run_cross_call_refund_tx(0x9a, 0);

        assert_eq!(restore_slot, Some(U256::from(1u64)), "restore should leave the original value");
        assert_eq!(reclear_slot, Some(U256::ZERO), "re-clear should leave the slot cleared");

        assert_eq!(restore_gas, base_tx().gas_limit, "the bill must be the declared limit");
        assert_eq!(reclear_gas, restore_gas, "a larger refund must not lower the bill");
    }

    /// Contract that burns `WARMTH_PROBE_PAD_GAS` in `JUMPDEST`s and stops, so a
    /// transaction calling it bills above the EIP-7623 calldata floor and
    /// smaller execution-gas differences stay observable.
    const FLOOR_BURNER: Address = address!("0x00000000000000000000000000000000000000e5");

    fn floor_burner_code() -> Bytes {
        let mut code = vec![0x5bu8; WARMTH_PROBE_PAD_GAS as usize];
        code.push(0x00);
        Bytes::from(code)
    }

    /// Signs a single-phase transaction calling the floor burner and then each
    /// of `targets` with no value.
    fn signed_root_calls(key: &SigningKey, targets: &[Address]) -> Eip8130Signed {
        let mut tx = base_tx();
        tx.calls = vec![
            core::iter::once(FLOOR_BURNER)
                .chain(targets.iter().copied())
                .map(|to| Call { to, value: U256::ZERO, data: Bytes::new() })
                .collect(),
        ];
        eoa_signed(tx, key)
    }

    /// Runs `signed` on the `eth_call` path with `coinbase` as the block
    /// beneficiary (warm per EIP-3651) and returns its metered gas (see
    /// [`metered_call`]). Comparing one signed transaction
    /// under two coinbases isolates the access cost of a target from the
    /// signature bytes' share of the payload cost.
    fn metered_gas_with_coinbase(
        signed: &Eip8130Signed,
        sender: Address,
        accounts: &[(Address, Bytes)],
        coinbase: Address,
    ) -> u64 {
        let mut contracts = vec![(FLOOR_BURNER, floor_burner_code())];
        contracts.extend_from_slice(accounts);
        let mut evm =
            evm_with_accounts(U256::from(10u64).pow(U256::from(18u64)), sender, &contracts);
        evm.ctx_mut().block.beneficiary = coinbase;
        let result = metered_call(&mut evm, signed, sender);
        assert!(result.is_success(), "expected success, got {result:?}");
        result.tx_gas_used()
    }

    /// Call-pool gas the floor burner spends ahead of the targets: its
    /// `JUMPDEST` padding. As the first call, its access is intrinsic.
    const FLOOR_BURNER_CALL_GAS: u64 = WARMTH_PROBE_PAD_GAS;

    /// Gas `signed` draws from the call pool when executed with `coinbase` as
    /// the block beneficiary: its metered gas minus its sender intrinsic gas.
    fn call_pool_gas_with_coinbase(
        signed: &Eip8130Signed,
        sender: Address,
        accounts: &[(Address, Bytes)],
        coinbase: Address,
    ) -> u64 {
        let intrinsic = IntrinsicGas::compute(signed, sender).expect("intrinsic gas");
        let metered = metered_gas_with_coinbase(signed, sender, accounts, coinbase);
        assert!(metered > intrinsic.sender_floor(), "metered gas must clear the calldata floor");
        metered - intrinsic.sender_intrinsic()
    }

    #[test]
    fn root_call_to_cold_target_charges_cold_account_access() {
        let key = signing_key(0xe1);
        let sender = eoa_address(&key);
        let target = address!("0x00000000000000000000000000000000000000cc");
        let signed = signed_root_calls(&key, &[target]);

        assert_eq!(
            call_pool_gas_with_coinbase(&signed, sender, &[], BENEFICIARY),
            FLOOR_BURNER_CALL_GAS + COLD_ACCOUNT_ACCESS_COST
        );
    }

    /// A root call to a warm target pays the warm access cost, as a `CALL` does.
    #[test]
    fn root_call_to_warm_target_charges_warm_account_access() {
        let key = signing_key(0xe5);
        let sender = eoa_address(&key);
        let target = address!("0x00000000000000000000000000000000000000cc");
        let signed = signed_root_calls(&key, &[target]);

        assert_eq!(
            call_pool_gas_with_coinbase(&signed, sender, &[], target),
            FLOOR_BURNER_CALL_GAS + WARM_STORAGE_READ_COST
        );
    }

    /// A repeated target pays the cold access once and the warm access after.
    #[test]
    fn repeated_root_call_target_pays_cold_then_warm_access() {
        let key = signing_key(0xe2);
        let sender = eoa_address(&key);
        let target = address!("0x00000000000000000000000000000000000000cc");
        let signed = signed_root_calls(&key, &[target, target]);

        assert_eq!(
            call_pool_gas_with_coinbase(&signed, sender, &[], BENEFICIARY),
            FLOOR_BURNER_CALL_GAS + COLD_ACCOUNT_ACCESS_COST + WARM_STORAGE_READ_COST
        );
    }

    /// A root call into an EIP-7702 delegated account also pays the access of
    /// the delegation target, cold or warm, as a `CALL` does.
    #[test]
    fn root_call_to_delegated_account_charges_delegate_access() {
        let key = signing_key(0xe3);
        let sender = eoa_address(&key);
        let delegated = address!("0x00000000000000000000000000000000000000cc");
        let delegate = address!("0x00000000000000000000000000000000000000cd");
        let accounts = [(delegated, Bytecode::new_eip7702(delegate).original_bytes())];
        let signed = signed_root_calls(&key, &[delegated]);

        let call_gas = |coinbase| call_pool_gas_with_coinbase(&signed, sender, &accounts, coinbase);
        let one_cold = FLOOR_BURNER_CALL_GAS + COLD_ACCOUNT_ACCESS_COST + WARM_STORAGE_READ_COST;
        assert_eq!(call_gas(BENEFICIARY), FLOOR_BURNER_CALL_GAS + 2 * COLD_ACCOUNT_ACCESS_COST);
        assert_eq!(call_gas(delegate), one_cold);
        assert_eq!(call_gas(delegated), one_cold);
    }

    /// Once a delegated account and its delegation target are both warm, a
    /// root call to it pays the warm access cost for each.
    #[test]
    fn root_call_to_warm_delegated_account_charges_two_warm_accesses() {
        let key = signing_key(0xe6);
        let sender = eoa_address(&key);
        let delegated = address!("0x00000000000000000000000000000000000000cc");
        let delegate = address!("0x00000000000000000000000000000000000000cd");
        let accounts = [(delegated, Bytecode::new_eip7702(delegate).original_bytes())];
        let signed = signed_root_calls(&key, &[delegated, delegated]);

        assert_eq!(
            call_pool_gas_with_coinbase(&signed, sender, &accounts, BENEFICIARY),
            FLOOR_BURNER_CALL_GAS + 2 * COLD_ACCOUNT_ACCESS_COST + 2 * WARM_STORAGE_READ_COST
        );
    }

    /// The estimate includes the root call access charge, so it equals the
    /// execution charge for a call to a cold or a warm target.
    #[test]
    fn simulate_estimate_includes_root_call_access() {
        let key = signing_key(0xe4);
        let sender = eoa_address(&key);
        let target = address!("0x00000000000000000000000000000000000000cc");
        let signed = signed_root_calls(&key, &[target]);

        let estimate = |coinbase: Address| {
            let mut evm = evm_with_accounts(
                U256::from(10u64).pow(U256::from(18u64)),
                sender,
                &[(FLOOR_BURNER, floor_burner_code())],
            );
            evm.ctx_mut().block.beneficiary = coinbase;
            evm.ctx_mut().tx = into_base_tx(&signed);
            evm.ctx_mut().tx.base.caller = sender;
            let result = Eip8130Executor::simulate(&mut evm).expect("estimation succeeds");
            assert!(result.is_success(), "expected success, got {result:?}");
            result.tx_gas_used()
        };

        let cold_estimate = estimate(BENEFICIARY);
        let warm_estimate = estimate(target);
        assert_eq!(
            cold_estimate - warm_estimate,
            COLD_ACCOUNT_ACCESS_COST - WARM_STORAGE_READ_COST
        );
        assert_eq!(cold_estimate, metered_gas_with_coinbase(&signed, sender, &[], BENEFICIARY));
        assert_eq!(warm_estimate, metered_gas_with_coinbase(&signed, sender, &[], target));
    }

    /// Executes and estimates a single-call transaction into a contract running
    /// `code`, returning `(executed gas used, estimated gas, gas limit)`.
    fn single_call_gas(signer: u8, code: Bytes) -> (u64, u64, u64) {
        let key = signing_key(signer);
        let sender = eoa_address(&key);
        let target = address!("0x00000000000000000000000000000000000000f1");
        let mut tx = base_tx();
        tx.calls = vec![vec![Call { to: target, value: U256::ZERO, data: Bytes::new() }]];
        let signed = eoa_signed(tx, &key);
        let fresh = || {
            evm_with_accounts(
                U256::from(10u64).pow(U256::from(18u64)),
                sender,
                &[(target, code.clone())],
            )
        };

        let outcome = fresh().transact_raw(into_base_tx(&signed)).expect("tx is included");
        assert!(!outcome.result.is_success(), "expected a failed phase");

        let mut evm = fresh();
        evm.ctx_mut().tx = into_base_tx(&signed);
        evm.ctx_mut().tx.base.caller = sender;
        let estimate = Eip8130Executor::simulate(&mut evm).expect("estimation runs");
        assert!(!estimate.is_success(), "expected a failed estimate");

        (outcome.result.tx_gas_used(), estimate.tx_gas_used(), signed.tx().gas_limit)
    }

    /// A root call that halts on `INVALID` consumes the whole call pool it was
    /// given, in execution and in the estimate.
    #[test]
    fn halting_root_call_consumes_its_whole_gas_allotment() {
        let (gas_used, estimate, gas_limit) = single_call_gas(0xd1, bytes!("fe"));
        assert_eq!(gas_used, gas_limit);
        assert_eq!(estimate, gas_limit);
    }

    /// A root call that reverts keeps its unused gas in the estimate, while
    /// gas-limit billing settles the declared limit.
    #[test]
    fn reverting_root_call_keeps_its_unused_gas() {
        let (gas_used, estimate, gas_limit) = single_call_gas(0xd2, bytes!("60006000fd"));
        assert!(estimate < gas_limit, "revert must not burn the pool: {estimate}");
        assert_eq!(gas_used, gas_limit);
    }

    /// A halt in a later phase burns the rest of the pool and skips every
    /// later phase, while an earlier committed phase keeps its writes.
    #[test]
    fn halting_root_call_keeps_earlier_phases_and_skips_later_ones() {
        let key = signing_key(0xd3);
        let sender = eoa_address(&key);
        let first = address!("0x00000000000000000000000000000000000000f2");
        let halter = address!("0x00000000000000000000000000000000000000f3");
        let last = address!("0x00000000000000000000000000000000000000f4");
        let call = |to| vec![Call { to, value: U256::ZERO, data: Bytes::new() }];
        let mut tx = base_tx();
        tx.calls = vec![call(first), call(halter), call(last)];
        let signed = eoa_signed(tx, &key);

        // PUSH1 1, PUSH1 0, SSTORE, STOP
        let storer = bytes!("600160005500");
        let mut evm = evm_with_accounts(
            U256::from(10u64).pow(U256::from(18u64)),
            sender,
            &[(first, storer.clone()), (halter, bytes!("fe")), (last, storer)],
        );
        let outcome = evm.transact_raw(into_base_tx(&signed)).expect("tx is included");

        assert!(matches!(outcome.result, ExecutionResult::Revert { .. }));
        assert_eq!(outcome.result.tx_gas_used(), signed.tx().gas_limit);
        assert_eq!(Eip8130CommittedPhases::take(), 1);
        let slot0 = |address: Address| {
            outcome
                .state
                .get(&address)
                .and_then(|account| account.storage.get(&U256::ZERO))
                .map(|slot| slot.present_value)
        };
        assert_eq!(slot0(first), Some(U256::from(1u64)), "committed phase keeps its write");
        assert!(slot0(last).is_none_or(|value| value.is_zero()), "later phase is skipped");
    }

    /// A root call starts with empty memory even after an earlier root call in
    /// the same phase expanded it, and pays memory expansion from zero.
    #[test]
    fn root_call_starts_with_empty_memory() {
        let key = signing_key(0xd4);
        let sender = eoa_address(&key);
        let writer = address!("0x00000000000000000000000000000000000000f5");
        let reader = address!("0x00000000000000000000000000000000000000f6");
        // MSTORE 0xff..ff at 0x00 and 0x60 (memory size 0x80), then RETURN(0, 0).
        let writer_code = bytes!(
            "7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff5f527fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff60605260005ff3"
        );
        // slot 0 = MSIZE; slot 1 = MLOAD(0); slot 2 = gas spent across
        // `PUSH0 MLOAD GAS`, read with two `GAS` opcodes.
        // MSIZE PUSH0 SSTORE GAS PUSH0 MLOAD GAS SWAP1 PUSH1 1 SSTORE SWAP1 SUB PUSH1 2 SSTORE STOP
        let reader_code = bytes!("595f555a5f515a90600155900360025500");
        // PUSH0 (2) + MLOAD (3) + one word of memory expansion (3) + GAS (2).
        const PUSH0_MLOAD_GAS_FROM_EMPTY_MEMORY: u64 = 2 + 3 + 3 + 2;

        let mut tx = base_tx();
        tx.calls = vec![vec![
            Call { to: writer, value: U256::ZERO, data: Bytes::new() },
            Call { to: reader, value: U256::ZERO, data: Bytes::new() },
        ]];
        let signed = eoa_signed(tx, &key);
        let mut evm = evm_with_accounts(
            U256::from(10u64).pow(U256::from(18u64)),
            sender,
            &[(writer, writer_code), (reader, reader_code)],
        );
        let outcome = evm.transact_raw(into_base_tx(&signed)).expect("tx is included");
        assert!(outcome.result.is_success(), "expected success, got {:?}", outcome.result);

        let slot = |index: u64| {
            outcome.state[&reader].storage.get(&U256::from(index)).map(|slot| slot.present_value)
        };
        assert_eq!(slot(0), Some(U256::ZERO), "MSIZE at entry");
        assert_eq!(slot(1), Some(U256::ZERO), "MLOAD(0) at entry");
        assert_eq!(slot(2), Some(U256::from(PUSH0_MLOAD_GAS_FROM_EMPTY_MEMORY)));
    }

    /// Estimation rejects a gas limit below intrinsic gas before it applies any
    /// account change: a malformed second create entry is never reached.
    #[test]
    fn simulate_checks_intrinsic_gas_before_applying_account_changes() {
        let key = signing_key(0xc9);
        let (derived, signed) = counterfactual_create_signed(&key, bytes!("00"), Vec::new());
        let mut tx = signed.tx().clone();
        let create = tx.account_changes[0].clone();
        tx.account_changes.push(create);
        tx.gas_limit = 1;
        let signed = configured_signed(tx, &key);

        let mut evm = with_zenith(evm_with(U256::from(10u64).pow(U256::from(18u64)), derived));
        let mut sim = into_base_tx(&signed);
        sim.base.caller = derived;
        if let Some(parts) = sim.eip8130.as_mut() {
            parts.mode = Eip8130ExecutionMode::Simulate;
        }
        evm.ctx_mut().tx = sim;
        let err = Eip8130Executor::simulate(&mut evm).unwrap_err();
        assert!(err.to_string().contains("below the EIP-7623 calldata floor"), "got {err}");
    }

    /// Installs a precompile at `address` that succeeds without output and
    /// counts how many times it is invoked.
    fn install_counting_precompile(
        evm: &mut BaseEvm<InMemoryDB, NoOpInspector, PrecompilesMap>,
        address: Address,
    ) -> Arc<AtomicUsize> {
        let runs = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&runs);
        evm.precompiles_mut().apply_precompile(&address, move |_| {
            Some(DynPrecompile::from(move |_: PrecompileInput<'_>| {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(PrecompileOutput::new(0, Bytes::new(), 0))
            }))
        });
        runs
    }

    /// An `eth_call` simulation runs the phased calls exactly once instead of
    /// repeating them for the gas-limit search.
    #[test]
    fn simulate_call_mode_executes_the_calls_once() {
        let key = signing_key(0xe8);
        let sender = eoa_address(&key);
        let counted = address!("0x00000000000000000000000000000000000000e8");
        let mut tx = base_tx();
        tx.calls = vec![vec![Call { to: counted, value: U256::ZERO, data: Bytes::new() }]];
        let signed = eoa_signed(tx, &key);

        let mut evm = evm_with(U256::from(10u64).pow(U256::from(18u64)), sender);
        let runs = install_counting_precompile(&mut evm, counted);
        let mut call = into_base_tx(&signed);
        call.base.caller = sender;
        if let Some(parts) = call.eip8130.as_mut() {
            parts.mode = Eip8130ExecutionMode::Call;
        }

        let outcome = evm.transact_raw(call).expect("call simulation runs");
        assert!(outcome.result.is_success(), "expected success, got {:?}", outcome.result);
        assert_eq!(runs.load(Ordering::SeqCst), 1, "eth_call must execute the calls once");
    }
}
