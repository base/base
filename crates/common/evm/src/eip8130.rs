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
//! Protocol-injected account-change logs (`ActorAuthorized`, `ActorRevoked`,
//! `AccountCreated`, `DelegationApplied`) are written to the journal during the
//! pre-call apply step and surface in the transaction receipt ahead of any
//! `calls` logs. Per-phase receipt status (`phaseStatuses`) is reported on the
//! EIP-8130 receipt; the overall transaction status (all-phases-succeeded vs
//! reverted) is reported through the returned [`ExecutionResult`] variant
//! ([`ExecutionResult::Success`] vs [`ExecutionResult::Revert`]).
//!
//! [transaction context]: TxContextStorage
//! [`BaseEvm::transact_raw`]: crate::BaseEvm

use alloc::{boxed::Box, rc::Rc, vec::Vec};

use alloy_evm::{Database as AlloyDatabase, EvmInternals};
use alloy_primitives::{Address, B256, Bytes, U256};
use base_common_consensus::{
    AccountChange, Delegation, Eip8130Constants, Eip8130Contracts, Predeploys,
};
use base_common_precompiles::{NonceManagerStorage, TxContextStorage};
use base_execution_eip8130::{
    AccountChangeApplier, AccountConfigurationEvents, AccountConfigurationStorage, ApplyError,
    DelegationEffect, FeeCheck, IntrinsicGas, IntrinsicGasInput, NonceMode, NonceValidator,
    TransactionAuthorizer,
};
use base_precompile_storage::{JournalStorageProvider, StorageCtx};
use revm::{
    Inspector,
    context::{BlockEnv, LocalContextTr, TxEnv, journaled_state::account::JournaledAccountTr},
    context_interface::{
        Block, Cfg, ContextTr, JournalTr,
        context::take_error,
        result::{EVMError, ExecutionResult, Output, ResultGas, SuccessReason},
    },
    handler::{EthFrame, EvmTr, FrameResult, Handler, PrecompileProvider},
    inspector::{InspectorEvmTr, InspectorHandler, JournalExt},
    interpreter::{
        CallInput, CallInputs, CallOutcome, CallScheme, CallValue, FrameInput, Gas,
        InstructionResult, InterpreterResult, SharedMemory, interpreter::EthInterpreter,
        interpreter_action::FrameInit,
    },
    primitives::{KECCAK_EMPTY, hardfork::SpecId},
    state::Bytecode,
};

use crate::{
    BaseContext, BaseContextTr, BaseEvm, BaseHaltReason, BaseSpecId, BaseTransaction,
    BaseTransactionError, BaseTxTr, Eip8130PhaseStatuses, L1BlockInfo, handler::BaseHandler,
};

/// EIP-3529 maximum gas refund quotient: refunds are capped at `gas_used / 5`.
/// Base is post-London, so this is constant across all live specs.
const MAX_REFUND_QUOTIENT: u64 = 5;

/// Maximum number of bisection steps the [`Eip8130Executor::simulate`] gas-limit
/// search runs before returning the tightest verified-feasible pool it has found.
/// The `POOL_SEARCH_TOLERANCE_PER_MILLE` early exit normally terminates the
/// search in far fewer steps; this is a hard backstop against pathological ranges.
const POOL_SEARCH_MAX_ITERS: u32 = 16;

/// Early-exit tolerance (in parts-per-thousand) for the gas-limit search: once the
/// `(highest - lowest)` window is within this fraction of `highest`, the search
/// stops and returns `highest` (a verified-feasible pool). Mirrors the standard
/// reth/geth estimator's 1.5% `ESTIMATE_GAS_ERROR_RATIO`.
const POOL_SEARCH_TOLERANCE_PER_MILLE: u64 = 15;

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
    /// Whether the authenticated sender actor has `SCOPE_POLICY`.
    pub policy_gated: bool,
    /// The policy gate target (`policy_manager(sender, actorId)`) resolved once
    /// at authorization; every `call.to` must equal this when policy-gated.
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
    /// Per-phase execution status, one entry per phase in `calls` and in phase
    /// order: `0x01` if the phase committed, `0x00` if it reverted or was skipped
    /// because an earlier phase reverted. Empty when `calls` was empty. This is
    /// the EIP-8130 `phaseStatuses` array surfaced through the receipt.
    phase_statuses: Vec<u8>,
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
        // Discard any phase statuses a previous transaction may have leaked into
        // the thread-local slot (e.g. via a panic caught between its `set` and the
        // receipt builder's `take`), so this transaction's receipt can only ever
        // observe its own statuses. See [`Eip8130PhaseStatuses`] panic safety.
        Eip8130PhaseStatuses::clear();

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

        // Consensus-critical cross-chain-replay guard. The sender and payer
        // signature hashes commit to the transaction body's *embedded* `chain_id`,
        // not the local chain, so a signature produced for another chain verifies
        // bit-for-bit when the identical bytes are executed here. `validate_static`
        // rejects a chain-id mismatch at pool admission, but block inclusion
        // (direct build / Engine delivery / singular-batch derivation, which
        // preserves the original raw bytes) bypasses the pool. The equality MUST be
        // enforced here — the enshrined pipeline is the only choke point every
        // inclusion path shares — so a foreign-chain envelope cannot advance the
        // sender's nonce or charge their balance locally. Unlike ordinary typed
        // transactions, the 8130 path bypasses revm's
        // `validate_against_state_and_deduct_caller` (see the L1BlockInfo refresh
        // below), so it does not inherit that path's chain-id check.
        if signed.tx().chain_id != chain_id {
            return Err(BaseTransactionError::eip8130("chain id mismatch").into());
        }

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

        let outcome =
            match Self::authorize_and_apply(ctx, &signed, &encoded, chain_id, now, base_fee) {
                Ok(outcome) => outcome,
                Err(err) => {
                    Self::discard_transaction_state(evm);
                    return Err(err.into());
                }
            };

        // Pre-charge the payer the worst-case fee (so `calls` cannot spend the
        // gas reservation), publish the transaction context, and run `calls`.
        let prepay = match Self::prepay(ctx, &outcome, &encoded, spec) {
            Ok(prepay) => prepay,
            Err(err) => {
                Self::discard_transaction_state(evm);
                return Err(err);
            }
        };

        // Mirror the mainnet handler's `load_accounts` pre-execution step, which
        // the 8130 path bypasses: set the journal's EVM spec id and warm the
        // precompiles and (EIP-3651) the coinbase before dispatching calls, so a
        // call is charged identically to one in a normal transaction on the same
        // chain.
        Self::warm_pre_call_accounts(evm);

        let inspection =
            Self::start_inspection(evm, outcome.sender, encoded.clone(), outcome.gas_limit);
        let mut calls =
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

        let gas_used = match Self::settle_fees(
            evm.ctx_mut(),
            &outcome,
            &calls,
            prepay,
            &encoded,
            spec,
            beneficiary,
        ) {
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

        // Hand the per-phase statuses to the receipt builder, which runs on this
        // same thread immediately after execution (see [`Eip8130PhaseStatuses`]).
        // This is the only channel available: the receipt builder is generic over
        // the EVM and the `ExecutionResult`'s `output` already carries the
        // transaction's revert data. Published as the last step before returning —
        // after the transaction is fully settled and committed — so neither a
        // `settle_fees` error nor a panic in the journal teardown above can leave
        // stale statuses in the slot for the next transaction; only the
        // allocation-free result construction below runs before the builder's
        // `take`.
        Eip8130PhaseStatuses::set(core::mem::take(&mut calls.phase_statuses));

        // The gas refund is already folded into `gas_used` (via `net_used` in
        // `settle_fees`), so the `refunded` counter is left 0.
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
    /// auto-delegation, intrinsic-gas, and phased-`calls` pipeline as
    /// [`Self::execute`] to measure gas, then reverts every journal write so no
    /// state is committed.
    ///
    /// Unlike [`Self::execute`] it performs **no signature verification** and
    /// **no fee settlement**: like `eth_call`/`eth_estimateGas` for every other
    /// transaction type, estimation simulates from the request's `from` without a
    /// signature. The sender actor and its policy are resolved from committed
    /// account state (not from a recovered signer), so the proof-of-recovery
    /// authorization token is never fabricated. This entrypoint is reachable only
    /// from the read-only RPC simulation path; block execution and txpool
    /// admission always go through [`Self::execute`] with full verification.
    ///
    /// Both the default-EOA (empty-`sender`) path and a configured sender are
    /// supported: a configured sender is resolved as its owner self-actor from
    /// committed state (the happy path), and the authentication gas of whichever
    /// authenticator the caller declared is priced from the synthesized
    /// `sender_auth` / `payer_auth` blob shape. No signature is ever verified, so
    /// the declared authenticator only selects which schedule entry is charged.
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
        // Clone the envelope + optional acting-actor hint before taking a mutable
        // borrow of `ctx` (same pattern as `execute`).
        let (signed, acting_actor_hint) = evm
            .ctx()
            .tx()
            .eip8130_parts()
            .map(|parts| (parts.signed.clone(), parts.simulation_sender_actor_id))
            .ok_or_else(|| {
                BaseTransactionError::eip8130("transaction is not an EIP-8130 transaction")
            })?;

        let ctx = evm.ctx_mut();
        let from = ctx.tx().base.caller;
        // Estimation skips authorization (no signature is verified), but it must
        // still apply account changes exactly as consensus would so the post-change
        // state the `calls` run against matches inclusion. That includes the
        // per-change JIT expiry skip, which depends on the block timestamp, so read
        // it here (in Unix seconds, the same clock as `ActorConfig::expiry`) and
        // thread it into `apply_account_changes`. Using the estimation block's
        // timestamp can only over-price a grant that expires before inclusion
        // (time moves forward, so a grant skipped now stays skipped) — it never
        // under-estimates.
        let now: u64 = ctx
            .block()
            .timestamp()
            .try_into()
            .map_err(|_| BaseTransactionError::eip8130("block timestamp exceeds u64"))?;
        let base_fee: u128 = u128::from(ctx.block().basefee());
        let encoded =
            ctx.tx().enveloped_tx().cloned().ok_or_else(|| {
                BaseTransactionError::eip8130("missing enveloped transaction bytes")
            })?;

        let outcome = match Self::simulate_resolve(
            ctx,
            &signed,
            &encoded,
            from,
            base_fee,
            acting_actor_hint,
            now,
        ) {
            Ok(outcome) => outcome,
            Err(err) => {
                Self::discard_transaction_state(evm);
                return Err(err.into());
            }
        };

        // Dispatch `calls` from the resolved sender so `tx.origin` reads correctly
        // (mirrors `prepay`'s caller overwrite on the execution path).
        evm.ctx_mut().tx.base.caller = outcome.sender;

        // Warm precompiles/coinbase and set the journal spec id exactly as
        // [`Self::execute`] does, so the simulated `calls` are charged the same
        // warm-access gas a real execution would — keeping the estimate aligned.
        Self::warm_pre_call_accounts(evm);
        let inspection =
            Self::start_inspection(evm, outcome.sender, encoded.clone(), outcome.gas_limit);

        // The estimate must return a gas *limit* that guarantees execution
        // succeeds, not the net charge. Two effects make that more than the gas a
        // single happy-path run consumes:
        //
        // 1. Refunds are credited to the payer *after* execution, so they never
        //    rejoin the call pool; the limit must cover the gross call spend.
        // 2. EIP-150 lets a `CALL` forward at most 63/64 of the gas available at
        //    the call site. The calls are measured here against a large pool (the
        //    request's `gas_limit`, defaulting to the block gas limit), so a
        //    contract that forwards "all but 1/64" across nested calls would, at a
        //    tighter `gas_limit == estimate`, forward less and could starve a deep
        //    callee — OOG-ing even though this simulation succeeded.
        //
        // Rather than guess a headroom factor, search for the minimum call pool at
        // which the calls still succeed — re-dispatching them at candidate pools
        // over fresh journal checkpoints (each reverted), exactly as standard
        // `eth_estimateGas` binary-searches the gas limit. The resolved sender,
        // applied account changes, and warmed accounts above are shared by every
        // probe, so only the call pool varies. Determinism of the EIP-8130 schedule
        // keeps the search to a handful of iterations.
        let ceiling_pool = outcome.execution_gas_available;

        // First run at the request's full pool: measures the baseline call spend
        // and decides whether the transaction can succeed at all. Probed under a
        // nested checkpoint so its writes (and logs) are rolled back before the
        // search reuses the resolved state.
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
            let final_calls = match Self::execute_calls(evm, &signed, &outcome, ceiling_pool) {
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
            let gross = outcome
                .sender_intrinsic
                .saturating_add(final_calls.call_gas_spent)
                .saturating_add(outcome.payer_auth);
            Self::end_inspection(
                evm,
                inspection,
                true,
                final_calls.output.clone(),
                outcome.gas_limit,
                gross,
            );
            Self::discard_transaction_state(evm);
            let result_gas = ResultGas::new_with_state_gas(gross, 0, 0, 0);
            return Ok(ExecutionResult::Revert {
                gas: result_gas,
                logs,
                output: final_calls.output,
            });
        }

        let feasible_pool = match Self::search_estimate_pool(
            evm,
            &signed,
            &outcome,
            ceiling.call_gas_spent,
            ceiling_pool,
        ) {
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

        // gas_limit = intrinsic + feasible_pool + payer_auth. The on-chain call
        // pool at this limit is `gas_limit - intrinsic = feasible_pool + payer_auth`
        // (payer authentication is billed on top of the limit, not drawn from the
        // pool), so it is at least `feasible_pool` — the verified-feasible amount —
        // and the limit also covers the net charge (which never exceeds it).
        let estimate_gas = outcome
            .sender_intrinsic
            .saturating_add(feasible_pool)
            .saturating_add(outcome.payer_auth);
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

    /// Runs the phased `calls` at `pool` under a nested journal checkpoint and
    /// reverts every write (and log) they made, leaving the journal at the
    /// resolved pre-call state so the next probe starts identically. Used only by
    /// the [`Self::simulate`] gas-limit search; returns the measured
    /// [`CallsResult`] so the caller can read `reverted` / `call_gas_spent`.
    fn probe_calls<DB, I, P>(
        evm: &mut BaseEvm<DB, I, P>,
        signed: &base_common_consensus::Eip8130Signed,
        outcome: &Eip8130Outcome,
        pool: u64,
    ) -> Result<CallsResult, EVMError<DB::Error, BaseTransactionError>>
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

    /// Searches for the minimum call pool at which the `calls` still succeed,
    /// given that they already succeeded at `ceiling_pool` consuming
    /// `ceiling_spent`. Returns a verified-feasible pool in
    /// `[ceiling_spent, ceiling_pool]`.
    ///
    /// Fast path: the measured spend is usually itself feasible (no gas lost to
    /// EIP-150 forwarding), so a single probe at `ceiling_spent` resolves it. Only
    /// when that probe fails does it bisect upward, seeded with the standard
    /// `64/63` optimistic guess and bounded by [`POOL_SEARCH_MAX_ITERS`] and the
    /// [`POOL_SEARCH_TOLERANCE_PER_MILLE`] early exit. Every bound assigned to
    /// `highest` is a probe-verified success, so the returned value always
    /// succeeds.
    fn search_estimate_pool<DB, I, P>(
        evm: &mut BaseEvm<DB, I, P>,
        signed: &base_common_consensus::Eip8130Signed,
        outcome: &Eip8130Outcome,
        ceiling_spent: u64,
        ceiling_pool: u64,
    ) -> Result<u64, EVMError<DB::Error, BaseTransactionError>>
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
        // The calls consumed `ceiling_spent` at the full pool, so no smaller pool
        // can satisfy them; if `ceiling_spent` itself succeeds it is the answer.
        let spent = ceiling_spent.min(ceiling_pool);
        if spent >= ceiling_pool || !Self::probe_calls(evm, signed, outcome, spent)?.reverted {
            return Ok(spent);
        }

        // `lowest` is a pool known (or assumed) to be insufficient; `highest` is a
        // verified-feasible pool. Bisect to shrink the window onto the threshold.
        let mut lowest = spent;
        let mut highest = ceiling_pool;

        // Optimistic 64/63 seed (covers one forwarding level), verified before use.
        let seed = spent.saturating_mul(64) / 63;
        if seed > lowest && seed < highest {
            if Self::probe_calls(evm, signed, outcome, seed)?.reverted {
                lowest = seed;
            } else {
                highest = seed;
            }
        }

        let mut iters = 0;
        while lowest + 1 < highest && iters < POOL_SEARCH_MAX_ITERS {
            // Stop once the window is within the tolerated fraction of `highest`,
            // returning `highest` (a verified-feasible pool).
            if (highest - lowest).saturating_mul(1000)
                <= highest.saturating_mul(POOL_SEARCH_TOLERANCE_PER_MILLE)
            {
                break;
            }
            let mid = lowest + (highest - lowest) / 2;
            if Self::probe_calls(evm, signed, outcome, mid)?.reverted {
                lowest = mid;
            } else {
                highest = mid;
            }
            iters += 1;
        }

        Ok(highest)
    }

    /// Resolves the [`Eip8130Outcome`] for [`Self::simulate`]: applies account
    /// changes, then resolves the acting actor (an optional RPC hint, else the
    /// account's self-actor) and its policy from the post-apply journal — no
    /// signature recovery — then auto-delegates and prices intrinsic gas without
    /// validating or advancing the nonce or checking the payer balance. The
    /// authentication gas for the sender's (and any payer's) declared
    /// authenticator is priced from the synthesized auth-blob shape via
    /// [`IntrinsicGas`]. Storage writes land on the journal; the caller's
    /// checkpoint reverts them.
    ///
    /// `acting_actor_hint` is the optional `senderActorId` from the estimate
    /// request. Without it, simulation publishes the self-actor (backward
    /// compatible).
    fn simulate_resolve<DB>(
        ctx: &mut BaseContext<DB>,
        signed: &base_common_consensus::Eip8130Signed,
        encoded: &[u8],
        sender: Address,
        base_fee: u128,
        acting_actor_hint: Option<B256>,
        now: u64,
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
        let nonce_key = tx.nonce_key;
        let gas_limit = tx.gas_limit;
        let max_fee = tx.max_fee_per_gas;
        let max_priority = tx.max_priority_fee_per_gas;
        // Use the declared payer (sponsor) so the published `TxContext` matches a
        // real execution: a call that reads the payer from the `TxContext`
        // precompile must see the same address it would on-chain, or it could take
        // a different path and skew the estimate. No signature is verified here.
        let payer = tx.payer.unwrap_or(sender);

        // Active runtime code-size cap (EIP-170 pre-Amsterdam, EIP-7954 after),
        // resolved before `ctx` is borrowed so estimation applies the same create
        // size limit as inclusion.
        let eth_spec: SpecId = ctx.cfg().spec().into();
        let max_code_size =
            Eip8130Constants::max_code_size(eth_spec.is_enabled_in(SpecId::AMSTERDAM));

        let internals = EvmInternals::from_context(ctx);
        let mut provider = JournalStorageProvider::new(internals, Address::ZERO);

        StorageCtx::enter(&mut provider, |sctx| {
            let nonce_mgr = NonceManagerStorage::new(sctx);

            // 1. Nonce-channel first-use flag (drives intrinsic gas). Estimation
            //    neither validates nor advances the nonce.
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

            // 2. Apply account changes and install deferred code effects so the
            //    calls run against post-change code and create/delegation gas is
            //    priced. Must precede actor/policy resolution so an actor
            //    authorized in this same estimate request is visible.
            let has_explicit_delegation =
                Self::apply_account_changes(signed, sctx, sender, now, max_code_size)?;

            // 3. Resolve the acting actor's real policy gate. No signature
            //    recovery: the optional RPC hint names the intended actor (e.g. a
            //    session key); absent that, fall back to the account's self-actor.
            //    Policy is read from the post-apply journal so same-tx
            //    authorizations are visible. Expiry is not enforced (estimation
            //    prices the happy path); `get_policy` still treats a revoked
            //    default-EOA self as ungated.
            //
            //    This real gate drives only the outcome's call-gating (whether
            //    `call.to` must equal `policy_target`); it does NOT feed the
            //    intrinsic-gas estimate, which pins the gate worst-case (step 5)
            //    so the returned ceiling stays valid even if the gate flips
            //    between estimation and inclusion (the gate is a non-monotonic
            //    state-dependent cost).
            let acc = AccountConfigurationStorage::new(sctx);
            let sender_actor_id = acting_actor_hint
                .unwrap_or_else(|| AccountConfigurationStorage::self_actor_id(sender));
            // Resolve the acting scope via the effective-config resolver: an
            // explicit `actor_config` entry, or the inline secp256k1 self (a
            // revoked default EOA resolves to the empty config, i.e. scope 0).
            // Then read the policy target with `get_policy_manager` only when
            // gated, avoiding `get_policy`'s extra `policy_commitment` SLOAD on
            // this estimation hot path (the commitment is unused here).
            let actor_scope = acc
                .resolve_actor_config(sender, sender_actor_id)
                .map_err(BaseTransactionError::eip8130)?
                .scope;
            let policy_gated = Eip8130Constants::sender_is_policy_gated(actor_scope);
            let policy_target = if policy_gated {
                acc.get_policy_manager(sender, sender_actor_id)
                    .map_err(BaseTransactionError::eip8130)?
            } else {
                Address::ZERO
            };

            // 4. Auto-delegate a code-less sender in the simulation state (so the
            //    calls run against a delegated sender), but *price* auto-delegation
            //    from the body-derivable worst case, not the sim-state result.
            //    Auto-delegation is non-monotonic — the sender's on-chain code can
            //    flip between estimation and inclusion — so pinning the body
            //    ceiling keeps the estimate a safe upper bound and, crucially,
            //    identical to what mempool admission pins. Resolving it from
            //    current code state here (while admission pins the body ceiling)
            //    would let admission exceed the estimate and reject a
            //    `gas_limit == estimate` submission. The state mutation stays gated
            //    on the absence of an explicit delegation (a zero target is an
            //    owner-authorized request to remain undelegated), matching the
            //    classifier's suppression on any `Delegation` entry.
            if !has_explicit_delegation {
                Self::auto_delegate_codeless_sender(sctx, sender)?;
            }
            let sender_auto_delegated =
                IntrinsicGasInput::sender_auto_delegated(&tx.account_changes);

            // 5. Intrinsic gas (auth gas is priced from the auth-blob shape, so a
            //    stub signature of the right authenticator type estimates exactly).
            //    The estimate is a safe ceiling that execution can only meet or
            //    undercharge. The non-monotonic, state-dependent costs are
            //    therefore pinned to their worst case rather than resolved:
            //      - both policy gates charged (their `policy_manager` SLOAD), so a
            //        `gas_limit == estimate` submission never OOGs if a gate flips
            //        on before inclusion. The payer's unsigned representative blob
            //        is not authenticable here in any case.
            //      - zero revoke discount, so revokes are priced at the full
            //        three-reset worst case regardless of which slots are empty.
            //      - auto-delegation pinned to the body ceiling above.
            //    The monotonic, body-derivable nonce first-use cost stays resolved.
            //    Execution reprices all of these precisely against the
            //    authenticated actors and real state.
            let (sender_intrinsic, payer_auth, execution_gas_available) =
                Self::resolve_execution_gas(
                    signed,
                    encoded,
                    &IntrinsicGasInput::worst_case(
                        nonce_key_first_use,
                        sender_auto_delegated,
                        tx.payer.is_some(),
                    ),
                    gas_limit,
                )?;

            // 6. Publish the transaction context for the `TxContext` precompile.
            TxContextStorage::new(sctx)
                .set_context(sender, payer, sender_actor_id)
                .map_err(BaseTransactionError::eip8130)?;

            Ok(Eip8130Outcome {
                sender,
                payer,
                sender_actor_id,
                policy_gated,
                policy_target,
                gas_limit,
                sender_intrinsic,
                payer_auth,
                execution_gas_available,
                effective: FeeCheck::effective_gas_price(max_fee, max_priority, base_fee),
                base_fee,
                bump_protocol_nonce: false,
            })
        })
    }

    /// Runs the storage-backed pre-call pipeline (authorize, nonce, intrinsic
    /// gas, fee-cap check, account-change apply, auto-delegation) over a gas-free
    /// journal view and publishes the transaction context, returning the resolved
    /// [`Eip8130Outcome`]. Storage writes land on the journal directly; the
    /// caller discards the transaction on error.
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
        let nonce_key = tx.nonce_key;
        let gas_limit = tx.gas_limit;
        let max_fee = tx.max_fee_per_gas;
        let max_priority = tx.max_priority_fee_per_gas;
        // The nonce-free replay ring records the transaction's upper validity
        // bound (`valid_before`, Unix milliseconds); the ring compares it against
        // `block.timestamp * 1000` internally.
        let valid_before = tx.valid_before;

        // Consensus-level validity window. The transaction is includable only
        // within the inclusive interval `[valid_after, valid_before]` on the
        // millisecond axis (`block.timestamp * 1000`). Per the EIP a transaction
        // is still valid at `now_ms == valid_before`, so the upper bound is
        // inclusive: reject only once `now_ms` is strictly past `valid_before`.
        // Enforced here so a block that includes a transaction outside its window
        // is invalid at execution, not merely filtered by the mempool; both bounds
        // apply to nonce-free and nonce-bearing transactions alike (`0` disables
        // the respective bound). The nonce-free replay ring separately enforces
        // its own admission window (`valid_before > now_ms`) when it records the
        // nonce, so a nonce-free transaction at the boundary still fails there.
        let now_ms = now.saturating_mul(1_000);
        if tx.valid_after != 0 && now_ms < tx.valid_after {
            return Err(BaseTransactionError::eip8130("transaction is not yet valid"));
        }
        if valid_before != 0 && now_ms > valid_before {
            return Err(BaseTransactionError::eip8130("transaction validity window has expired"));
        }

        // Runtime code-size cap for enshrined creates: track the EVM's active
        // limit (EIP-170 pre-Amsterdam, EIP-7954 from Amsterdam/Denim) so an
        // EIP-8130 create is held to the same cap as an ordinary `CREATE`.
        // Resolved before `ctx` is borrowed by the storage provider below.
        let eth_spec: SpecId = ctx.cfg().spec().into();
        let max_code_size =
            Eip8130Constants::max_code_size(eth_spec.is_enabled_in(SpecId::AMSTERDAM));

        let internals = EvmInternals::from_context(ctx);
        let mut provider = JournalStorageProvider::new(internals, Address::ZERO);

        StorageCtx::enter(&mut provider, |sctx| {
            // Ordering note: the apply step (1) and code effects (2) write journal
            // storage *before* the nonce is validated (3). Any `Err` returned from
            // this closure propagates out of `authorize_and_apply` and the caller
            // discards the transaction, so these earlier writes never persist for a
            // rejected transaction. This mirrors the caller-MUST-discard contract
            // documented on `TransactionAuthorizer::authorize_and_apply`.
            let mut acc = AccountConfigurationStorage::new(sctx);

            // 1. Authorize and apply the account changes interleaved against the
            //    evolving state, then authenticate sender/payer against the
            //    resulting post-apply state. `AccountConfiguration` storage
            //    transitions are written here; the deferred account-code effects
            //    are installed in step 2.
            let applied_tx = TransactionAuthorizer::authorize_and_apply(
                signed,
                &mut acc,
                chain_id,
                now,
                max_code_size,
            )
            .map_err(BaseTransactionError::eip8130)?;
            let has_explicit_delegation = applied_tx.applied.delegation.is_some();
            let sender_actor = applied_tx.actors.sender.resolved;
            let payer_policy_gated = applied_tx
                .actors
                .payer
                .as_ref()
                .is_some_and(|actor| actor.resolved.is_policy_gated());
            let sender = applied_tx.actors.sender.account;
            let payer = applied_tx.actors.payer.as_ref().map_or(sender, |p| p.account);
            // Defense-in-depth: `authorize_and_apply` -> `verify_sender` already
            // gates `can_use_nonce_key(nonce_key)` on both the configured and
            // EOA sender paths, so this is redundant on the current call graph. It
            // is kept as a local guard so this execution entry point stays sound if
            // the sender-resolution path is ever refactored to skip that check.
            if !sender_actor.can_use_nonce_key(nonce_key) {
                return Err(BaseTransactionError::eip8130(
                    "sender actor scope does not authorize sequenced nonces",
                ));
            }

            // 2. Install the deferred account-*code* effects (created-account
            //    bytecode, delegation indicator) the apply step surfaced.
            if let Some(created) = &applied_tx.applied.created {
                Self::install_created_code(sctx, created.address, &created.code)?;
            }
            if let Some(delegation) = &applied_tx.applied.delegation {
                delegation.install(sctx).map_err(BaseTransactionError::eip8130)?;
            }

            // 3. Validate and advance the nonce.
            let mut nonce_mgr = NonceManagerStorage::new(sctx);
            let protocol_nonce = sctx
                .with_account_info(sender, |info| Ok(info.nonce))
                .map_err(BaseTransactionError::eip8130)?;
            // The nonce-free replay lookup works in milliseconds (`now_ms`,
            // `block.timestamp * 1000`), matching the validity window and the
            // ring buffer; the sequence-channel branches ignore this argument.
            let (nonce_key_first_use, bump_protocol_nonce) =
                if nonce_key == Eip8130Constants::NONCE_KEY_MAX {
                    NonceValidator::validate(
                        tx,
                        sender,
                        protocol_nonce,
                        &nonce_mgr,
                        NonceMode::Inclusion,
                        now_ms,
                    )
                    .map_err(BaseTransactionError::eip8130)?;
                    let replay = NonceValidator::replay_hash(tx, sender);
                    nonce_mgr
                        .check_and_mark_expiring_nonce(replay, valid_before)
                        .map_err(BaseTransactionError::eip8130)?;
                    (false, false)
                } else if nonce_key == U256::ZERO {
                    NonceValidator::validate(
                        tx,
                        sender,
                        protocol_nonce,
                        &nonce_mgr,
                        NonceMode::Inclusion,
                        now_ms,
                    )
                    .map_err(BaseTransactionError::eip8130)?;
                    (protocol_nonce == 0, true)
                } else {
                    let current_nonce = nonce_mgr
                        .get_nonce(sender, nonce_key)
                        .map_err(BaseTransactionError::eip8130)?;
                    NonceValidator::validate_sequence(tx, current_nonce, NonceMode::Inclusion)
                        .map_err(BaseTransactionError::eip8130)?;
                    nonce_mgr
                        .increment_nonce(sender, nonce_key)
                        .map_err(BaseTransactionError::eip8130)?;
                    (current_nonce == 0, false)
                };

            // 4. Auto-delegate a code-less sender only when no explicit
            //    delegation owner change was supplied. A zero target deliberately
            //    clears the sender's delegation and must not be overwritten with
            //    `DEFAULT_ACCOUNT`.
            // A create installs the account's own runtime, so it must never be
            // auto-delegated to `DEFAULT_ACCOUNT`. `apply_create` rejects empty
            // runtimes, so a created sender is never code-less here and
            // `auto_delegate_codeless_sender` would already no-op; gating on the
            // create explicitly keeps that invariant local rather than relying on
            // the emptiness check, and matches the `sender_auto_delegated`
            // intrinsic-gas classifier's own suppression on a create entry.
            let has_create = applied_tx.applied.created.is_some();
            let sender_auto_delegated = if has_explicit_delegation || has_create {
                false
            } else {
                Self::auto_delegate_codeless_sender(sctx, sender)?
            };

            // 5. Intrinsic gas under the EIP-8130 schedule.
            let (sender_intrinsic, payer_auth, execution_gas_available) =
                Self::resolve_execution_gas(
                    signed,
                    encoded,
                    &IntrinsicGasInput::new(nonce_key_first_use, sender_auto_delegated)
                        .with_policy_gates(sender_actor.is_policy_gated(), payer_policy_gated)
                        .with_revoke_discount_slots(applied_tx.revoke_discount_slots),
                    gas_limit,
                )?;

            // 6. Fee caps and payer balance.
            FeeCheck::validate_fees(max_fee, max_priority, base_fee)
                .map_err(BaseTransactionError::eip8130)?;
            let payer_balance = sctx
                .with_account_info(payer, |info| Ok(info.balance))
                .map_err(BaseTransactionError::eip8130)?;
            FeeCheck::validate_balance(payer_balance, gas_limit, payer_auth, max_fee)
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
                policy_gated: sender_actor.is_policy_gated(),
                policy_target: sender_actor.policy_target,
                gas_limit,
                sender_intrinsic,
                payer_auth,
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
    /// from a single gas `pool`. Each phase runs under a journal checkpoint: a
    /// successful phase commits and its gas refund counts; a reverting phase (or
    /// one blocked by the policy gate) rolls back, is charged for the gas already
    /// consumed without refund, and skips every later phase.
    ///
    /// `pool` is the gas available to the calls (`gas_limit - sender_intrinsic`).
    /// Block execution passes `outcome.execution_gas_available`; the read-only
    /// estimate probes the same calls at several candidate pools to search for the
    /// minimum gas limit that succeeds, so it is supplied explicitly rather than
    /// read from `outcome`.
    fn execute_calls<DB, I, P>(
        evm: &mut BaseEvm<DB, I, P>,
        signed: &base_common_consensus::Eip8130Signed,
        outcome: &Eip8130Outcome,
        pool: u64,
    ) -> Result<CallsResult, EVMError<DB::Error, BaseTransactionError>>
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
        let mut remaining = pool;
        // Signed transaction-level refund counter: refunds are accounted across
        // the whole transaction, not per call. See [`CallsResult::refund`].
        let mut refund: i64 = 0;
        let total_phases = signed.tx().calls.len();
        // One status byte per phase; phases not reached after a revert are filled
        // with `0x00` below.
        let mut phase_statuses: Vec<u8> = Vec::with_capacity(total_phases);

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
                if outcome.policy_gated && call.to != outcome.policy_target {
                    phase_reverted = true;
                    phase_output =
                        Self::actor_policy_violation_data(outcome.sender_actor_id, call.to);
                    break;
                }

                let frame =
                    Self::run_call(evm, outcome.sender, call.to, call.data.clone(), remaining)?;
                let gas = frame.gas();
                // `run_call` caps the frame at `remaining`, so a call can never
                // report spending more than the pool held; treat a violation of
                // that EVM invariant as a hard error rather than silently clamping.
                remaining = remaining.checked_sub(gas.total_gas_spent()).ok_or_else(|| {
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
                // This phase reverted; record it and report every remaining
                // (unexecuted) phase as reverted too, per EIP-8130.
                phase_statuses.push(0x00);
                phase_statuses.resize(total_phases, 0x00);
                return Ok(CallsResult {
                    call_gas_spent: pool.saturating_sub(remaining),
                    refund,
                    reverted: true,
                    output: phase_output,
                    phase_statuses,
                });
            }

            // revm's `checkpoint_commit` merges the phase savepoint into its
            // parent without finalizing the journal entries, so a committed phase
            // is still rolled back if the transaction is ultimately discarded
            // (see `discard_transaction_state`) — e.g. when a subsequent phase
            // surfaces a database error. Committed phases are only durable once
            // `commit_tx` runs.
            evm.ctx_mut().journal_mut().checkpoint_commit();
            phase_statuses.push(0x01);
            refund = refund.saturating_add(phase_refund);
        }

        Ok(CallsResult {
            call_gas_spent: pool.saturating_sub(remaining),
            refund,
            reverted: false,
            output: Bytes::new(),
            phase_statuses,
        })
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
        let mut gas = Gas::new(gas_limit);
        let _ = gas.record_regular_cost(gas_used.min(gas_limit));
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

    /// Dispatches a single protocol call (`from = sender`, `value = 0`) as a
    /// top-level EVM call frame with `gas_limit` and runs it to completion,
    /// returning the [`FrameResult`]. Reuses the Base handler's frame loop and
    /// drives the configured inspector when inspection is enabled.
    fn run_call<DB, I, P>(
        evm: &mut BaseEvm<DB, I, P>,
        caller: Address,
        to: Address,
        data: Bytes,
        gas_limit: u64,
    ) -> Result<FrameResult, EVMError<DB::Error, BaseTransactionError>>
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
        let frame = if evm.inspect {
            handler.inspect_run_exec_loop(evm, frame_init)?
        } else {
            handler.run_exec_loop(evm, frame_init)?
        };

        // A top-level frame's database error is recorded on the context and the
        // interpreter halts with `FatalExternalError`; unlike a nested frame
        // (whose error is surfaced when its outcome is folded into the parent
        // via `EthFrame::return_result`), the root frame is returned directly by
        // `run_exec_loop` without that check. Mirror the mainnet
        // `Handler::execution_result` guard so a node-local DB failure raised
        // mid-call propagates as a fatal `Err` instead of being misread as a
        // deterministic call revert on this consensus-critical path.
        take_error::<EVMError<DB::Error, BaseTransactionError>, _>(evm.ctx_mut().error())?;

        Ok(frame)
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
    fn warm_pre_call_accounts<DB, I, P>(evm: &mut BaseEvm<DB, I, P>)
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
        // Sender-intrinsic + call gas, less the EIP-3529-capped refund, plus payer
        // authentication. Shared with the estimate path so they cannot diverge.
        let billable_gas = Self::billable_gas(outcome, calls);

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

    /// The gas billed for an EIP-8130 transaction: sender-intrinsic plus the gas
    /// its `calls` consumed, less the EIP-3529-capped refund, plus payer
    /// authentication (billed on top of the sender budget).
    ///
    /// The refund-cap denominator (`gross_used`) includes `sender_intrinsic` —
    /// like mainnet, where intrinsic gas counts toward the `gas_used / 5` ceiling —
    /// but deliberately excludes `payer_auth`: payer authentication carries no
    /// SSTORE/SELFDESTRUCT refund of its own, so it must not inflate the refund
    /// ceiling. (Mainnet has no payer-auth concept, so this is an EIP-8130-specific
    /// choice rather than literal mainnet parity.)
    ///
    /// This is the **net consensus charge** used by [`Self::settle_fees`]. The
    /// read-only estimate ([`Self::simulate`]) uses the gross amount instead —
    /// `sender_intrinsic + call_gas_spent + payer_auth` — because refunds are
    /// credited after execution and are never available to the call pool during
    /// execution, so the gas limit must cover the full gross spend.
    fn billable_gas(outcome: &Eip8130Outcome, calls: &CallsResult) -> u64 {
        let gross_used = outcome.sender_intrinsic.saturating_add(calls.call_gas_spent);
        let refund = Self::capped_refund(calls.refund, gross_used);
        let net_used = gross_used.saturating_sub(refund);
        net_used.saturating_add(outcome.payer_auth)
    }

    /// Applies the transaction's account-configuration changes and installs the
    /// deferred account-*code* effects (created-account code and delegation),
    /// directly on the journal-backed storage — *without* authenticating the
    /// changes. Used by the read-only estimation pipeline
    /// ([`Self::simulate_resolve`]) so the post-change code the `calls` run
    /// against matches inclusion. The verifying pipeline instead routes through
    /// [`TransactionAuthorizer::authorize_and_apply`], which interleaves the same
    /// application with authorization against the evolving state.
    fn apply_account_changes(
        signed: &base_common_consensus::Eip8130Signed,
        sctx: StorageCtx<'_>,
        sender: Address,
        now: u64,
        max_code_size: usize,
    ) -> Result<bool, BaseTransactionError> {
        let mut acc_mut = AccountConfigurationStorage::new(sctx);
        let mut created_effect: Option<(Address, Bytes)> = None;
        let mut delegation_effect: Option<DelegationEffect> = None;
        for (index, change) in signed.tx().account_changes.iter().enumerate() {
            match change {
                AccountChange::Create(entry) => {
                    if delegation_effect.is_some() {
                        return Err(BaseTransactionError::eip8130(ApplyError::CreateAndDelegation));
                    }
                    if index != 0 || created_effect.is_some() {
                        return Err(BaseTransactionError::eip8130(
                            ApplyError::InvalidCreatePosition,
                        ));
                    }
                    let created =
                        AccountChangeApplier::apply_create(&mut acc_mut, entry, max_code_size)
                            .map_err(BaseTransactionError::eip8130)?;
                    created_effect = Some((created.address, created.code));
                }
                AccountChange::ConfigChange(cc) => {
                    // Estimation prices revokes at the worst-case three-reset cost
                    // (a zero revoke discount is pinned), so the resolved
                    // empty-slot count is applied but not needed here. `now` is the
                    // block timestamp (Unix seconds) so the JIT expiry skip matches
                    // consensus: a lapsed unsequenced grant is dropped in both the
                    // estimate and at inclusion, keeping the post-change state (and
                    // therefore the simulated `calls`) aligned.
                    AccountChangeApplier::apply_config_change(
                        &mut acc_mut,
                        sender,
                        &cc.changes,
                        cc.channel,
                        cc.sequence,
                        now,
                    )
                    .map_err(BaseTransactionError::eip8130)?;
                }
                AccountChange::Delegation(Delegation { target }) => {
                    if delegation_effect.is_some() {
                        return Err(BaseTransactionError::eip8130(ApplyError::MultipleDelegations));
                    }
                    if created_effect.is_some() {
                        return Err(BaseTransactionError::eip8130(ApplyError::CreateAndDelegation));
                    }
                    delegation_effect = Some(DelegationEffect::new(sender, *target));
                }
            }
        }
        if let Some((address, code)) = &created_effect {
            Self::install_created_code(sctx, *address, code)?;
        }
        let has_explicit_delegation = delegation_effect.is_some();
        if let Some(delegation) = delegation_effect {
            delegation.install(sctx).map_err(BaseTransactionError::eip8130)?;
        }
        Ok(has_explicit_delegation)
    }

    /// Installs a created account's runtime code, enforcing the CREATE2 collision
    /// rule the reference contract gets for free from a real deploy: the
    /// destination must be empty (no code, zero nonce). The account info is read
    /// from the real journal (not the config overlay), so block inclusion
    /// enforces what mempool admission checks separately — an inclusion path that
    /// bypasses the pool cannot overwrite preexisting third-party code.
    ///
    /// The runtime is already validated non-empty, `<= MAX_CODE_SIZE`, and not
    /// `0xEF`-prefixed by [`AccountChangeApplier::apply_create`], so
    /// [`Bytecode::new_raw_checked`] never errors here; the fallible constructor
    /// is used anyway so any future gap surfaces as a validity error rather than
    /// a panic on transaction-controlled bytes.
    fn install_created_code(
        sctx: StorageCtx<'_>,
        address: Address,
        code: &Bytes,
    ) -> Result<(), BaseTransactionError> {
        let occupied = sctx
            .with_account_info(address, |info| Ok(!info.is_empty_code_hash() || info.nonce != 0))
            .map_err(BaseTransactionError::eip8130)?;
        if occupied {
            return Err(BaseTransactionError::eip8130(
                "create destination already has code or a non-zero nonce",
            ));
        }
        let bytecode =
            Bytecode::new_raw_checked(code.clone()).map_err(BaseTransactionError::eip8130)?;
        sctx.set_code(address, bytecode).map_err(BaseTransactionError::eip8130)
    }

    /// Auto-delegates a code-less sender to [`Eip8130Contracts::DEFAULT_ACCOUNT`]
    /// so the account can dispatch its `calls`, returning whether the delegation
    /// was installed (which feeds the intrinsic-gas schedule). The verifying and
    /// estimation paths call this only when the transaction has no explicit
    /// delegation change; an owner-authorized zero target must remain cleared.
    /// A configured account already has code, so this is otherwise a no-op for it.
    fn auto_delegate_codeless_sender(
        sctx: StorageCtx<'_>,
        sender: Address,
    ) -> Result<bool, BaseTransactionError> {
        let is_codeless = sctx
            .with_account_info(sender, |info| Ok(info.is_empty_code_hash()))
            .map_err(BaseTransactionError::eip8130)?;
        if is_codeless {
            let target = Eip8130Contracts::DEFAULT_ACCOUNT;
            sctx.set_code(sender, Bytecode::new_eip7702(target))
                .map_err(BaseTransactionError::eip8130)?;
            // Same protocol-injected receipt log as an explicit delegation entry.
            AccountConfigurationEvents::emit_delegation_applied(sctx, sender, target)
                .map_err(BaseTransactionError::eip8130)?;
        }
        Ok(is_codeless)
    }

    /// Computes the EIP-8130 intrinsic gas and the gas left for `calls`, returning
    /// `(sender_intrinsic, payer_auth, execution_gas_available)`. Shared by both
    /// pipelines so intrinsic pricing is computed identically for execution and
    /// estimation. Errors when sender-intrinsic gas exceeds the gas limit.
    fn resolve_execution_gas(
        signed: &base_common_consensus::Eip8130Signed,
        encoded: &[u8],
        input: &IntrinsicGasInput,
        gas_limit: u64,
    ) -> Result<(u64, u64, u64), BaseTransactionError> {
        let intrinsic =
            IntrinsicGas::compute(signed, encoded, input).map_err(BaseTransactionError::eip8130)?;
        let execution_gas_available =
            intrinsic.execution_gas_available(gas_limit).ok_or_else(|| {
                BaseTransactionError::eip8130("EIP-8130 sender-intrinsic gas exceeds the gas limit")
            })?;
        Ok((intrinsic.sender_intrinsic(), intrinsic.payer_auth, execution_gas_available))
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
    use alloy_sol_types::{SolEvent, SolValue, sol};
    use base_common_consensus::{
        AccountChange, AccountChangeChannel, BaseTxEnvelope, Call, ChangeType, CreateEntry,
        Eip8130Signed, InitialActor, Predeploys, SignedAccountChanges, SignedChange, TxEip8130,
    };
    use base_common_precompiles::INonceManager;
    use base_execution_eip8130::{AccountChangeApplier, DelegationApplied};
    use base_precompile_storage::{HashMapStorageProvider, StorageCtx};
    use k256::ecdsa::SigningKey;
    use revm::{
        Database,
        bytecode::Bytecode,
        context::{BlockEnv, CfgEnv, Context},
        database::InMemoryDB,
        database_interface::DBErrorMarker,
        inspector::NoOpInspector,
        state::AccountInfo,
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

    #[test]
    fn auto_delegate_codeless_sender_delegates_to_default_account() {
        // A codeless sender is auto-delegated to `DEFAULT_ACCOUNT` so it can
        // dispatch its calls; a sender that already has code is left untouched.
        let plain = address!("0x00000000000000000000000000000000000000c1");
        let coded = address!("0x00000000000000000000000000000000000000c2");
        let mut provider = HashMapStorageProvider::new(CHAIN_ID);
        StorageCtx::enter(&mut provider, |ctx| {
            assert!(
                Eip8130Executor::auto_delegate_codeless_sender(ctx, plain).unwrap(),
                "codeless EOA must be auto-delegated"
            );
            ctx.set_code(coded, Bytecode::new_raw(Bytes::from_static(&[0x60, 0x00]))).unwrap();
            assert!(
                !Eip8130Executor::auto_delegate_codeless_sender(ctx, coded).unwrap(),
                "a sender with code must not be auto-delegated"
            );
        });

        assert_eq!(
            provider
                .get_account_info(plain)
                .and_then(|info| info.code.as_ref())
                .and_then(Bytecode::eip7702_address),
            Some(Eip8130Contracts::DEFAULT_ACCOUNT),
            "codeless sender must delegate to DEFAULT_ACCOUNT"
        );
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

        let log = outcome
            .result
            .logs()
            .iter()
            .find(|log| log.address == NonceManagerStorage::ADDRESS)
            .expect("nonce increment event");
        let event = INonceManager::NonceIncremented::decode_log_data(&log.data).unwrap();
        assert_eq!(event.account, sender);
        assert_eq!(event.nonceKey, nonce_key);
        assert_eq!(event.newNonce, current_nonce + 1);
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
            Bytecode::new_eip7702(Eip8130Contracts::DEFAULT_ACCOUNT).original_bytes(),
        );

        let outcome = evm.transact_raw(into_base_tx(&signed)).expect("clear should execute");
        let sender_acc = outcome.state.get(&sender).expect("sender in state");
        assert!(sender_acc.info.is_empty_code_hash(), "explicit zero target must remain cleared");

        let ExecutionResult::Success { logs, .. } = &outcome.result else {
            panic!("expected successful clear, got {:?}", outcome.result);
        };
        assert_eq!(logs.len(), 1, "clear must not be followed by auto-delegation");
        assert_eq!(logs[0].address, AccountConfigurationStorage::ADDRESS);
        let event = DelegationApplied::decode_log_data(&logs[0].data).unwrap();
        assert_eq!(event.account, sender);
        assert_eq!(event.target, Address::ZERO);
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
    fn simulate_explicit_zero_delegation_emits_only_clear() {
        let key = signing_key(0x26);
        let sender = eoa_address(&key);
        let mut tx = base_tx();
        tx.account_changes = vec![AccountChange::Delegation(Delegation { target: Address::ZERO })];
        let signed = eoa_signed(tx, &key);
        let mut evm = evm_with(U256::from(10u64).pow(U256::from(18u64)), sender);
        seed_account_code(
            &mut evm,
            sender,
            Bytecode::new_eip7702(Eip8130Contracts::DEFAULT_ACCOUNT).original_bytes(),
        );
        evm.ctx_mut().tx = into_base_tx(&signed);
        evm.ctx_mut().tx.base.caller = sender;

        let result = Eip8130Executor::simulate(&mut evm).expect("clear should simulate");
        let ExecutionResult::Success { logs, .. } = result else {
            panic!("expected successful simulation, got {result:?}");
        };
        assert_eq!(logs.len(), 1, "clear must not be followed by auto-delegation");
        let event = DelegationApplied::decode_log_data(&logs[0].data).unwrap();
        assert_eq!(event.account, sender);
        assert_eq!(event.target, Address::ZERO);
    }

    #[test]
    fn simulate_estimate_covers_execution_gas_without_a_signature() {
        let key = signing_key(0x77);
        let sender = eoa_address(&key);
        let target = address!("0x00000000000000000000000000000000000000c5");

        let mut tx = base_tx();
        tx.calls = vec![vec![Call { to: target, data: Bytes::new() }]];
        let signed = eoa_signed(tx, &key);
        let initial = U256::from(10u64).pow(U256::from(18u64));

        // Reference: execute the fully-signed transaction and read its charged gas.
        let mut evm_exec = evm_with_accounts(initial, sender, &[(target, bytes!("00"))]);
        let exec_gas = evm_exec
            .transact_raw(into_base_tx(&signed))
            .expect("tx should execute")
            .result
            .gas()
            .tx_gas_used();

        // Estimate over the same shape with the sender supplied as `from` (the
        // signature is never recovered on the simulate path).
        let mut evm_sim = evm_with_accounts(initial, sender, &[(target, bytes!("00"))]);
        evm_sim.ctx_mut().tx = into_base_tx(&signed);
        evm_sim.ctx_mut().tx.base.caller = sender;
        let sim_result =
            Eip8130Executor::simulate(&mut evm_sim).expect("estimation should succeed");
        let sim_gas = sim_result.tx_gas_used();

        assert!(sim_result.is_success(), "estimation should report success");
        assert!(sim_gas > 0, "estimated gas should be positive");
        // The estimate is a gas *limit* that must cover the real execution charge
        // (a safe ceiling execution can only meet or undercharge). This
        // transaction calls a STOP contract (no nested calls, no SSTORE/
        // SELFDESTRUCT), so it loses no gas to EIP-150 forwarding and earns no
        // refund. The only gap is the non-monotonic sender policy gate, which the
        // estimate pins worst-case: this EOA sender is ungated, so the estimate
        // exceeds the execution charge by exactly one pinned `policy_manager`
        // COLD_SLOAD and by nothing else.
        assert_eq!(
            sim_gas,
            exec_gas + base_execution_eip8130::Eip8130GasSchedule::COLD_SLOAD,
            "estimate must be the execution charge plus exactly the pinned policy-gate SLOAD",
        );

        // Estimation never commits: a fresh execution after it still bumps the
        // nonce from zero, proving no nonce was consumed by the simulation.
        let mut evm_after = evm_with_accounts(initial, sender, &[(target, bytes!("00"))]);
        let after = evm_after.transact_raw(into_base_tx(&signed)).expect("tx should execute");
        assert_eq!(after.state.get(&sender).expect("sender").info.nonce, 1);
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
    fn simulate_estimate_covers_gross_gas_when_sstore_refund_earned() {
        // Proof that the estimate returns the GROSS call spend, not the net
        // billable charge. The target contract clears storage slot 0 (pre-seeded
        // to 1), earning an EIP-3529 SSTORE_CLEARS refund (~4800 gas). The old
        // code returned `billable_gas` (call_gas_spent − capped_refund) as the
        // estimate; using that as gas_limit leaves the call pool one refund-unit
        // short, causing OOG. The fixed estimate returns the gross amount
        // (intrinsic + call_gas_spent, no refund subtracted). Assertions:
        //   - estimate_gas > charge_gas (gross > net when a refund is earned)
        //   - executing at gas_limit = charge_gas reverts (pool too small)
        //   - executing at gas_limit = estimate_gas succeeds
        //
        // Note: `charge_gas` here is `billable_gas` from a real execution run —
        // the net consensus charge that the old code wrongly used as the estimate.
        // The refund (~4800 gas) swamps any calldata-encoding variance between the
        // two gas-limit values (~16 gas), so the `charge_gas` run reliably OOGs.
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

        // --- reference execution at a generous limit to obtain the net charge ---
        let mut tx_ref = base_tx();
        tx_ref.calls = vec![vec![Call { to: target, data: Bytes::new() }]];
        let signed_ref = eoa_signed(tx_ref, &key);
        let mut evm_ref = evm_with_accounts_and_storage(
            initial,
            sender,
            &[(target, sstore_clears.clone())],
            &storage,
        );
        let charge_gas = evm_ref
            .transact_raw(into_base_tx(&signed_ref))
            .expect("tx should execute")
            .result
            .tx_gas_used();

        // --- estimate (simulation never commits state) ---
        let mut tx_sim = base_tx();
        tx_sim.calls = vec![vec![Call { to: target, data: Bytes::new() }]];
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

        // The gross estimate must strictly exceed the net charge because a
        // refund was earned.
        assert!(
            estimate_gas > charge_gas,
            "estimate_gas ({estimate_gas}) must exceed net charge ({charge_gas}): \
             refund must not reduce the gas limit"
        );

        // --- execute at gas_limit = charge_gas → must revert (OOG: refund was
        //     subtracted from pool but is not available during execution) ---
        let mut tx_low = base_tx();
        tx_low.gas_limit = charge_gas;
        tx_low.calls = vec![vec![Call { to: target, data: Bytes::new() }]];
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
        tx_ok.calls = vec![vec![Call { to: target, data: Bytes::new() }]];
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
        // net billable charge, the 1/64 retained by the forwarder at each hop
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

        // --- reference execution at a generous limit for the net charge ---
        let mut tx_ref = base_tx();
        tx_ref.calls = vec![vec![Call { to: forwarder, data: Bytes::new() }]];
        let signed_ref = eoa_signed(tx_ref, &key);
        let mut evm_ref = evm_with_accounts(
            initial,
            sender,
            &[(forwarder, fwd_code.clone()), (sink, sink_code.clone())],
        );
        let charge_gas = evm_ref
            .transact_raw(into_base_tx(&signed_ref))
            .expect("reference execution should succeed")
            .result
            .tx_gas_used();

        // --- estimate (binary search must go beyond ceiling_spent) ---
        let mut tx_sim = base_tx();
        tx_sim.calls = vec![vec![Call { to: forwarder, data: Bytes::new() }]];
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
            "estimate_gas ({estimate_gas}) must exceed the net charge ({charge_gas}): \
             the 63/64 retention requires a higher gas limit than the raw spend"
        );

        // --- execute at gas_limit = charge_gas → must revert (sink OOGs) ---
        let mut tx_low = base_tx();
        tx_low.gas_limit = charge_gas;
        tx_low.calls = vec![vec![Call { to: forwarder, data: Bytes::new() }]];
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
        tx_ok.calls = vec![vec![Call { to: forwarder, data: Bytes::new() }]];
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
        tx.calls = vec![vec![Call { to: allowed, data: Bytes::new() }]];
        let signed = configured_signed(tx, &owner);

        let mut evm = evm_with_accounts(
            U256::from(10u64).pow(U256::from(18u64)),
            account,
            &[(allowed, bytes!("00")), (wrong, bytes!("00"))],
        );
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
        tx.calls = vec![vec![Call { to: target, data: Bytes::new() }]];
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

        let err = evm.transact_raw(into_base_tx(&signed)).unwrap_err();
        assert!(
            matches!(err, EVMError::Database(_)),
            "DB failure during a call must surface as a fatal database error, got {err:?}",
        );
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
            tx.calls = vec![vec![Call { to: target, data: Bytes::new() }]];
            let signed = eoa_signed(tx, &key);
            let mut evm = evm_with_accounts(
                U256::from(10u64).pow(U256::from(18u64)),
                sender,
                &[(target, code)],
            );
            let outcome = evm.transact_raw(into_base_tx(&signed)).expect("call should execute");
            assert!(outcome.result.is_success(), "expected success, got {:?}", outcome.result);
            outcome.result.gas().tx_gas_used()
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
        // PUSH1 0, SLOAD, STOP
        let loader_code = bytes!("60005400");

        let mut evm = evm_with_accounts(
            U256::from(10u64).pow(U256::from(18u64)),
            sender,
            &[(loader, loader_code)],
        );

        let mut warm_tx = base_tx();
        warm_tx.calls = vec![vec![Call { to: loader, data: Bytes::new() }]];
        let warm_signed = eoa_signed(warm_tx, &key);
        let warm_outcome =
            evm.transact_raw(into_base_tx(&warm_signed)).expect("tx should be included");
        assert!(matches!(warm_outcome.result, ExecutionResult::Success { .. }));
        revm::DatabaseCommit::commit(evm.ctx_mut().journal_mut().db_mut(), warm_outcome.state);

        // Tx 1 already bumped the protocol nonce (0 -> 1) and delegated the sender
        // (both committed above), so tx 2 is a second-use transaction:
        // base AA_BASE_COST 15_000
        // payload EIP-2028 DA over the tx 1_700
        // nonce_key existing channel 0: COLD_SLOAD 2_100 + SSTORE_RESET 2_900 5_000
        // auto_delegation sender already delegated 0
        // sender_auth ECRECOVER 3_000 + cold SLOAD 2_100 5_100
        // call PUSH1 (3) + COLD SLOAD (2_100) + STOP 2_103
        // total 28_903
        let mut load_tx = base_tx();
        load_tx.nonce_sequence = 1;
        load_tx.calls = vec![vec![Call { to: loader, data: Bytes::new() }]];
        let load_signed = eoa_signed(load_tx, &key);
        let load_outcome =
            evm.transact_raw(into_base_tx(&load_signed)).expect("tx should be included");
        assert!(matches!(load_outcome.result, ExecutionResult::Success { .. }));

        assert_eq!(
            load_outcome.result.gas().tx_gas_used(),
            28_903,
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
        // PUSH1 0, SLOAD, STOP
        let loader_code = bytes!("60005400");

        let mut evm = evm_with_accounts(
            U256::from(10u64).pow(U256::from(18u64)),
            sender,
            &[(loader, loader_code)],
        );

        let mut tx = base_tx();
        tx.calls = vec![
            vec![Call { to: loader, data: Bytes::new() }],
            vec![Call { to: loader, data: Bytes::new() }],
        ];
        let signed = eoa_signed(tx, &key);
        let outcome = evm.transact_raw(into_base_tx(&signed)).expect("tx should be included");
        assert!(matches!(outcome.result, ExecutionResult::Success { .. }));

        // First-use, single-tx, two phases each calling `loader`:
        // base AA_BASE_COST 15_000
        // payload EIP-2028 DA over the two-phase tx 1_856
        // nonce_key first use of channel 0: COLD_SLOAD 2_100 + SSTORE_SET 20_000 22_100
        // auto_delegation codeless EOA -> DEFAULT_ACCOUNT 4_600
        // sender_auth ECRECOVER 3_000 + cold SLOAD 2_100 5_100
        // call phase 0 PUSH1 (3) + COLD SLOAD (2_100) + STOP 2_103
        // call phase 1 PUSH1 (3) + WARM SLOAD (100) + STOP 103
        // total 50_862
        assert_eq!(
            outcome.result.gas().tx_gas_used(),
            50_862,
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
        // PUSH1 0, SLOAD, STOP
        let loader_code = bytes!("60005400");

        let mut evm = evm_with_accounts(
            U256::from(10u64).pow(U256::from(18u64)),
            sender,
            &[(loader, loader_code)],
        );

        let mut invalid_tx = base_tx();
        invalid_tx.nonce_sequence = 5;
        invalid_tx.calls = vec![vec![Call { to: loader, data: bytes!("60006000fd") }]];
        let invalid_signed = eoa_signed(invalid_tx, &key);
        let invalid_outcome = evm.transact_raw(into_base_tx(&invalid_signed)).unwrap_err();
        assert!(
            matches!(invalid_outcome, EVMError::Transaction(BaseTransactionError::Eip8130(_))),
            "validation error should surface as a validation error, got {invalid_outcome:?}",
        );

        let mut load_tx = base_tx();
        load_tx.calls = vec![vec![Call { to: loader, data: Bytes::new() }]];
        let load_signed = eoa_signed(load_tx, &key);
        let load_outcome =
            evm.transact_raw(into_base_tx(&load_signed)).expect("tx should be included");
        assert!(matches!(load_outcome.result, ExecutionResult::Success { .. }));

        // `load_tx` is a self-paying EOA transaction with one phase calling
        // `loader` (PUSH1 0, SLOAD, STOP). Its gas splits into the EIP-8130
        // sender-intrinsic charge (48_484) plus the dispatched call (2_103):
        // base AA_BASE_COST 15_000
        // payload EIP-2028 DA over the 122-byte tx 1_688
        // nonce_key first use of channel 0: COLD_SLOAD 2_100 + SSTORE_SET 20_000 22_100
        // auto_delegation codeless EOA -> DEFAULT_ACCOUNT (200 x 23-byte indicator) 4_600
        // sender_auth ECRECOVER 3_000 + cold SLOAD 2_100 5_100
        // call PUSH1 (3) + SLOAD + STOP (0) 2_103
        // total 50_591
        assert_eq!(
            load_outcome.result.gas().tx_gas_used(),
            50_591,
            "loader SLOAD must be COLD (2_100); a warm read (100) would total \
             48_591, meaning the discarded invalid tx leaked warmth",
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
        // flashblock. Non-empty runtime code mirrors the on-chain account.
        let key = signing_key(0xc1);
        let (derived, signed) = counterfactual_create_signed(&key, bytes!("00"), Vec::new());

        let initial_balance = U256::from(10u64).pow(U256::from(18u64));
        let mut evm = evm_with(initial_balance, derived);
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
        let mut evm = evm_with(U256::from(10u64).pow(U256::from(18u64)), derived);
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
        let mut evm = evm_with(U256::from(10u64).pow(U256::from(18u64)), derived);
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
            vec![vec![Call { to: target, data: Bytes::new() }]],
        );

        let initial_balance = U256::from(10u64).pow(U256::from(18u64));
        let mut evm = evm_with_accounts(initial_balance, derived, &[(target, bytes!("00"))]);
        let outcome = evm
            .transact_raw(into_base_tx(&signed))
            .expect("counterfactual create + call should execute");
        assert!(outcome.result.is_success(), "expected success, got {:?}", outcome.result);
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
