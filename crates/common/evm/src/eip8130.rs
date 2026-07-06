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
use base_common_consensus::{
    AccountChange, Delegation, Eip8130Constants, Eip8130Contracts, Predeploys,
};
use base_common_precompiles::{NonceManagerStorage, TxContextStorage};
use base_execution_eip8130::{
    AccountChangeApplier, AccountConfigurationStorage, ApplyError, FeeCheck, IntrinsicGas,
    IntrinsicGasInput, NonceMode, NonceValidator, TransactionAuthorizer,
};
use base_precompile_storage::{JournalStorageProvider, StorageCtx};
use revm::{
    context::{
        BlockEnv, LocalContextTr, TxEnv,
        journaled_state::{JournalCheckpoint, account::JournaledAccountTr},
    },
    context_interface::{
        Block, Cfg, ContextTr, JournalTr,
        context::take_error,
        result::{EVMError, ExecutionResult, Output, ResultGas, SuccessReason},
    },
    handler::{EthFrame, EvmTr, FrameResult, Handler, PrecompileProvider},
    interpreter::{
        CallInput, CallInputs, CallScheme, CallValue, FrameInput, InterpreterResult, SharedMemory,
        interpreter::EthInterpreter, interpreter_action::FrameInit,
    },
    primitives::hardfork::SpecId,
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
        P: PrecompileProvider<BaseContext<DB>, Output = InterpreterResult>,
        BaseContext<DB>: BaseContextTr
            + ContextTr<
                Db = DB,
                Tx = BaseTransaction<TxEnv>,
                Block = BlockEnv,
                Journal: core::fmt::Debug,
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

        // Mirror the mainnet handler's `load_accounts` pre-execution step, which
        // the 8130 path bypasses: set the journal's EVM spec id and warm the
        // precompiles and (EIP-3651) the coinbase before dispatching calls, so a
        // call is charged identically to one in a normal transaction on the same
        // chain.
        Self::warm_pre_call_accounts(evm);

        let mut calls =
            match Self::execute_calls(evm, &signed, &outcome, outcome.execution_gas_available) {
                Ok(calls) => calls,
                Err(err) => {
                    Self::teardown_after_error(evm, checkpoint);
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
                Self::teardown_after_error(evm, checkpoint);
                return Err(err);
            }
        };

        let ctx = evm.ctx_mut();

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
        P: PrecompileProvider<BaseContext<DB>, Output = InterpreterResult>,
        BaseContext<DB>: BaseContextTr
            + ContextTr<
                Db = DB,
                Tx = BaseTransaction<TxEnv>,
                Block = BlockEnv,
                Journal: core::fmt::Debug,
            >,
    {
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
        let from = ctx.tx().base.caller;
        // Estimation prices the happy path: it skips authorization, so it does not
        // enforce expiry, and the EIP-8130 intrinsic schedule does not depend on
        // the block timestamp — so no timestamp is read here (unlike `execute`).
        let base_fee: u128 = u128::from(ctx.block().basefee());
        let encoded =
            ctx.tx().enveloped_tx().cloned().ok_or_else(|| {
                BaseTransactionError::eip8130("missing enveloped transaction bytes")
            })?;

        let checkpoint = ctx.journal_mut().checkpoint();
        let outcome = match Self::simulate_resolve(ctx, &signed, &encoded, from, base_fee) {
            Ok(outcome) => outcome,
            Err(err) => {
                ctx.journal_mut().checkpoint_revert(checkpoint);
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
        let ceiling = match Self::probe_calls(evm, &signed, &outcome, ceiling_pool) {
            Ok(ceiling) => ceiling,
            Err(err) => {
                Self::teardown_after_error(evm, checkpoint);
                return Err(err);
            }
        };

        // A revert/halt even at the full pool is a genuine failure (not a gas
        // shortfall the search could fix): surface it like standard
        // `eth_estimateGas`. Re-run once un-reverted to capture the revert output
        // and any logs the committed phases emitted before the failing phase.
        if ceiling.reverted {
            let final_calls = match Self::execute_calls(evm, &signed, &outcome, ceiling_pool) {
                Ok(final_calls) => final_calls,
                Err(err) => {
                    Self::teardown_after_error(evm, checkpoint);
                    return Err(err);
                }
            };
            let logs = evm.ctx_mut().journal_mut().take_logs();
            evm.ctx_mut().journal_mut().checkpoint_revert(checkpoint);
            let gross = outcome
                .sender_intrinsic
                .saturating_add(final_calls.call_gas_spent)
                .saturating_add(outcome.payer_auth);
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
                Self::teardown_after_error(evm, checkpoint);
                return Err(err);
            }
        };

        // Final canonical run at the chosen pool (un-reverted) to capture the logs
        // and output at the returned gas limit, then discard all simulated state.
        let final_calls = match Self::execute_calls(evm, &signed, &outcome, feasible_pool) {
            Ok(final_calls) => final_calls,
            Err(err) => {
                Self::teardown_after_error(evm, checkpoint);
                return Err(err);
            }
        };
        let logs = evm.ctx_mut().journal_mut().take_logs();
        evm.ctx_mut().journal_mut().checkpoint_revert(checkpoint);

        // gas_limit = intrinsic + feasible_pool + payer_auth. The on-chain call
        // pool at this limit is `gas_limit - intrinsic = feasible_pool + payer_auth`
        // (payer authentication is billed on top of the limit, not drawn from the
        // pool), so it is at least `feasible_pool` — the verified-feasible amount —
        // and the limit also covers the net charge (which never exceeds it).
        let estimate_gas = outcome
            .sender_intrinsic
            .saturating_add(feasible_pool)
            .saturating_add(outcome.payer_auth);
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
        P: PrecompileProvider<BaseContext<DB>, Output = InterpreterResult>,
        BaseContext<DB>: BaseContextTr
            + ContextTr<
                Db = DB,
                Tx = BaseTransaction<TxEnv>,
                Block = BlockEnv,
                Journal: core::fmt::Debug,
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
        P: PrecompileProvider<BaseContext<DB>, Output = InterpreterResult>,
        BaseContext<DB>: BaseContextTr
            + ContextTr<
                Db = DB,
                Tx = BaseTransaction<TxEnv>,
                Block = BlockEnv,
                Journal: core::fmt::Debug,
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

    /// Resolves the [`Eip8130Outcome`] for [`Self::simulate`]: derives the
    /// sender's owner self-actor from `sender` and reads its policy from
    /// committed account state (no signature recovery), then applies account
    /// changes, auto-delegates, and prices intrinsic gas — without validating or
    /// advancing the nonce or checking the payer balance. The authentication gas
    /// for the sender's (and any payer's) declared authenticator is priced from
    /// the synthesized auth-blob shape via [`IntrinsicGas`]. Storage writes land
    /// on the journal; the caller's checkpoint reverts them.
    fn simulate_resolve<DB>(
        ctx: &mut BaseContext<DB>,
        signed: &base_common_consensus::Eip8130Signed,
        encoded: &[u8],
        sender: Address,
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
        // Use the declared payer (sponsor) so the published `TxContext` matches a
        // real execution: a call that reads the payer from the `TxContext`
        // precompile must see the same address it would on-chain, or it could take
        // a different path and skew the estimate. No signature is verified here.
        let payer = tx.payer.unwrap_or(sender);

        let internals = EvmInternals::from_context(ctx);
        let mut provider = JournalStorageProvider::new(internals, Address::ZERO);

        StorageCtx::enter(&mut provider, |sctx| {
            let acc = AccountConfigurationStorage::new(sctx);
            let nonce_mgr = NonceManagerStorage::new(sctx);

            // 1. Resolve the default-EOA self actor from committed account state.
            //    No signature recovery: revocation/expiry are not enforced here
            //    because estimation prices the happy-path gas.
            let sender_actor_id = AccountConfigurationStorage::self_actor_id(sender);
            let state = acc.get_account_state(sender).map_err(BaseTransactionError::eip8130)?;
            let policy_type = state.default_eoa_policy_type;
            let policy_target = if policy_type == 0 {
                Address::ZERO
            } else {
                acc.get_policy_manager(sender, sender_actor_id)
                    .map_err(BaseTransactionError::eip8130)?
            };

            // 2. Nonce-channel first-use flag (drives intrinsic gas). Estimation
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

            // 3. Apply account changes and install deferred code effects so the
            //    calls run against post-change code and create/delegation gas is
            //    priced.
            Self::apply_account_changes(signed, sctx, sender)?;

            // 4. Auto-delegate a code-less sender to the default account. Unlike
            //    the verifying path this is unconditional on a code-less sender: a
            //    configured account already has code, so the check is a no-op for
            //    it, and a basic-account sender is delegated to `DEFAULT_ACCOUNT`.
            let sender_auto_delegated = Self::auto_delegate_codeless_sender(sctx, sender)?;

            // 5. Intrinsic gas (auth gas is priced from the auth-blob shape, so a
            //    stub signature of the right authenticator type estimates exactly).
            let (sender_intrinsic, payer_auth, execution_gas_available) =
                Self::resolve_execution_gas(
                    signed,
                    encoded,
                    nonce_key_first_use,
                    sender_auto_delegated,
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
                policy_type,
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
        let nonce_key = tx.nonce_key;
        let gas_limit = tx.gas_limit;
        let max_fee = tx.max_fee_per_gas;
        let max_priority = tx.max_priority_fee_per_gas;
        let expiry = tx.expiry;
        let sender_sig_hash = tx.sender_signature_hash();

        let internals = EvmInternals::from_context(ctx);
        let mut provider = JournalStorageProvider::new(internals, Address::ZERO);

        StorageCtx::enter(&mut provider, |sctx| {
            // Ordering note: the apply step (1) and code effects (2) write journal
            // storage *before* the nonce is validated (3). Any `Err` returned from
            // this closure propagates out of `authorize_and_apply` and is reverted
            // wholesale by the caller's journal checkpoint in `execute` (taken
            // before this call, reverted on error), so these earlier writes never
            // persist for a rejected transaction. This mirrors the
            // caller-MUST-discard contract documented on
            // `TransactionAuthorizer::authorize_and_apply`.
            let mut acc = AccountConfigurationStorage::new(sctx);

            // 1. Authorize and apply the account changes interleaved against the
            //    evolving state, then authenticate sender/payer against the
            //    resulting post-apply state. `AccountConfiguration` storage
            //    transitions are written here; the deferred account-code effects
            //    are installed in step 2.
            let applied_tx =
                TransactionAuthorizer::authorize_and_apply(signed, &mut acc, chain_id, now)
                    .map_err(BaseTransactionError::eip8130)?;
            let sender_actor = applied_tx.actors.sender.resolved;
            let sender = applied_tx.actors.sender.account;
            let payer = applied_tx.actors.payer.as_ref().map_or(sender, |p| p.account);

            // 2. Install the deferred account-*code* effects (created-account
            //    bytecode, delegation indicator) the apply step surfaced.
            if let Some(created) = &applied_tx.applied.created {
                sctx.set_code(created.address, Bytecode::new_raw(created.code.clone()))
                    .map_err(BaseTransactionError::eip8130)?;
            }
            if let Some(delegation) = &applied_tx.applied.delegation {
                let code = if delegation.target.is_zero() {
                    Bytecode::default()
                } else {
                    Bytecode::new_eip7702(delegation.target)
                };
                sctx.set_code(delegation.account, code).map_err(BaseTransactionError::eip8130)?;
            }

            // 3. Resolve the nonce channel's first-use flag and validate the nonce.
            let mut nonce_mgr = NonceManagerStorage::new(sctx);
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

            // 4. Advance the nonce. The protocol (basic-account) nonce is bumped
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

            // 5. Auto-delegate any code-less sender to the default account so the
            // account can dispatch its calls. An explicit delegation applied in
            // step 2 with a non-zero target leaves non-empty code and is preserved
            // here. Clearing the sender's delegation in the same transaction leaves
            // it code-less and is intentionally re-delegated — any basic-account
            // sender is always delegated to `DEFAULT_ACCOUNT` regardless of which
            // signing key or authenticator was used.
            let sender_auto_delegated = Self::auto_delegate_codeless_sender(sctx, sender)?;

            // 6. Intrinsic gas under the EIP-8130 schedule.
            let (sender_intrinsic, payer_auth, execution_gas_available) =
                Self::resolve_execution_gas(
                    signed,
                    encoded,
                    nonce_key_first_use,
                    sender_auto_delegated,
                    gas_limit,
                )?;

            // 7. Fee caps and payer balance.
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
                policy_type: sender_actor.policy_type,
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
        P: PrecompileProvider<BaseContext<DB>, Output = InterpreterResult>,
        BaseContext<DB>: BaseContextTr
            + ContextTr<
                Db = DB,
                Tx = BaseTransaction<TxEnv>,
                Block = BlockEnv,
                Journal: core::fmt::Debug,
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
                if outcome.policy_type != 0 && call.to != outcome.policy_target {
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
            // is still rolled back if the outer checkpoint (taken in `execute`)
            // later reverts — e.g. when a subsequent phase surfaces a database
            // error. Committed phases are only durable once `commit_tx` runs.
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
        let frame = handler.run_exec_loop(evm, frame_init)?;

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

    /// Centralized post-error teardown mirroring the mainnet handler's
    /// `catch_error` cleanup: reverts the transaction checkpoint, clears the
    /// cached L1 cost, and drains the frame stack and local context. A database
    /// error raised inside a nested subcall surfaces while the parent frame is
    /// still on the stack, so draining both prevents stale frame/local state
    /// from leaking into the next transaction when a `BaseEvm` is reused.
    fn teardown_after_error<DB, I, P>(evm: &mut BaseEvm<DB, I, P>, checkpoint: JournalCheckpoint)
    where
        DB: AlloyDatabase,
        P: PrecompileProvider<BaseContext<DB>, Output = InterpreterResult>,
        BaseContext<DB>:
            BaseContextTr + ContextTr<Db = DB, Tx = BaseTransaction<TxEnv>, Block = BlockEnv>,
    {
        let ctx = evm.ctx_mut();
        ctx.journal_mut().checkpoint_revert(checkpoint);
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
    ) -> Result<(), BaseTransactionError> {
        let mut acc_mut = AccountConfigurationStorage::new(sctx);
        let mut created_effect: Option<(Address, Bytes)> = None;
        let mut delegation_effect: Option<(Address, Address)> = None;
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
                    let created = AccountChangeApplier::apply_create(&mut acc_mut, entry)
                        .map_err(BaseTransactionError::eip8130)?;
                    created_effect = Some((created.address, created.code));
                }
                AccountChange::ConfigChange(cc) => {
                    AccountChangeApplier::apply_config_change(
                        &mut acc_mut,
                        sender,
                        &cc.actor_changes,
                        cc.chain_id,
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
                    delegation_effect = Some((sender, *target));
                }
            }
        }
        if let Some((address, code)) = created_effect {
            sctx.set_code(address, Bytecode::new_raw(code))
                .map_err(BaseTransactionError::eip8130)?;
        }
        if let Some((account, target)) = delegation_effect {
            let code =
                if target.is_zero() { Bytecode::default() } else { Bytecode::new_eip7702(target) };
            sctx.set_code(account, code).map_err(BaseTransactionError::eip8130)?;
        }
        Ok(())
    }

    /// Auto-delegates a code-less sender to [`Eip8130Contracts::DEFAULT_ACCOUNT`]
    /// so the account can dispatch its `calls`, returning whether the delegation
    /// was installed (which feeds the intrinsic-gas schedule). Both the verifying
    /// and estimation paths call this unconditionally: a configured account
    /// already has code so the check is a no-op for it, and any basic-account
    /// sender — regardless of signing path or authenticator — is delegated to
    /// `DEFAULT_ACCOUNT`.
    fn auto_delegate_codeless_sender(
        sctx: StorageCtx<'_>,
        sender: Address,
    ) -> Result<bool, BaseTransactionError> {
        let is_codeless = sctx
            .with_account_info(sender, |info| Ok(info.is_empty_code_hash()))
            .map_err(BaseTransactionError::eip8130)?;
        if is_codeless {
            sctx.set_code(sender, Bytecode::new_eip7702(Eip8130Contracts::DEFAULT_ACCOUNT))
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
        nonce_key_first_use: bool,
        sender_auto_delegated: bool,
        gas_limit: u64,
    ) -> Result<(u64, u64, u64), BaseTransactionError> {
        let intrinsic = IntrinsicGas::compute(
            signed,
            encoded,
            &IntrinsicGasInput::new(nonce_key_first_use, sender_auto_delegated),
        )
        .map_err(BaseTransactionError::eip8130)?;
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
    use base_common_consensus::{
        AccountChange, BaseTxEnvelope, Call, CreateEntry, Eip8130Signed, InitialActor, Predeploys,
        TxEip8130,
    };
    use base_execution_eip8130::AccountChangeApplier;
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
        // The estimate is a gas *limit* that must cover the real execution charge.
        // This transaction calls a STOP contract (no nested calls, no
        // SSTORE/SELFDESTRUCT), so it loses no gas to EIP-150 forwarding and earns
        // no refund: the gas-limit search converges on exactly the gas a real
        // execution charges, so the estimate equals `exec_gas`.
        assert_eq!(sim_gas, exec_gas, "no-forwarding estimate should equal the execution charge");

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
            id[..20].copy_from_slice(owner.as_slice());
            B256::from_slice(&id)
        };
        let initial_actors =
            vec![InitialActor { actor_id, authenticator: Eip8130Constants::K1_AUTHENTICATOR }];
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
        // (PR #3766): a `0x7b` create whose sender is the not-yet-existent CREATE2
        // address must authorize and be *included* through the full
        // `Eip8130Executor::execute` pipeline — not just the unit-level
        // `authorize_and_apply`. Before the fix this returned
        // `BaseTransactionError::Eip8130("...NotBound")` and was rejected at every
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
