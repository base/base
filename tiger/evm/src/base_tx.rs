//! Enshrined Base transaction (type `0x79`, Everest) execution.
//!
//! A port of the revm path's `TxBaseExecutor` (`crates/common/evm/src/base_tx.rs`). A Base
//! transaction is not one EVM call: a *pre-call pipeline* authorizes the sender and payer,
//! validates and advances the transaction's 2D nonce, charges the EIP-8430 intrinsic gas
//! schedule, checks the fee caps, installs the delegation entry, pre-charges the payer and
//! publishes the transaction context; then the transaction's `calls` run as top-level call
//! frames, phase by phase, from one gas pool; then the fee is settled.
//!
//! The pre-call pipeline is gas-free (the intrinsic schedule prices it), but it reads and writes
//! state through the transaction overlay exactly where the reference reads its journal, so the
//! same accounts and storage slots are warm when the calls run.
//!
//! `calls` is a list of phases, each a list of calls. Phases commit independently in order; the
//! calls within one phase are all-or-nothing. A reverting or halting call rolls back its phase
//! and skips every later phase; the transaction is still included (nonce consumed, fee paid) with
//! a failed status. The receipt reports how many leading phases committed
//! ([`BaseTxResultExt::committed_phases`]).
//!
//! Each call is dispatched from the sender to `call.to` with `msg.value == call.value` and
//! `tx.origin == sender`, given the whole remaining pool. Dispatch first draws EIP-2929 account
//! access gas from the pool as a `CALL` would (the first call's target access is intrinsic), plus
//! [`TxBaseGasSchedule::NEW_ACCOUNT_COST`] for a value transfer that creates its target.
//!
//! Gas-limit billing ([`BaseFeature::GasLimitBilling`], active with Base transactions) bills
//! and reports the reserved budget, `gas_limit + payer_auth_cost`, whatever the calls spent.
//! With fee charging disabled (`eth_call`), nothing is charged and the metered gas is reported.
//!
//! [`TxBaseExecutor::simulate`] is the read-only `eth_call` / `eth_estimateGas` path: no
//! signature checks, no fee settlement, nothing committed.

use alloy_eips::eip2718::{Encodable2718, Typed2718};
use alloy_primitives::{Address, B256, Bytes, U256, address};
use base_common_base_tx::{IntrinsicGas, TxBaseGasSchedule};
use base_common_consensus::{
    AccountChange, Delegation, Predeploys, TxBase, TxBaseConstants, TxBaseSigned,
};
use base_common_genesis::BaseFeature;
use evm2::{
    Evm, EvmFeatures, TxResult,
    bytecode::Bytecode,
    env::{TxEnv, TxEnvExt},
    inspector::Inspector,
    interpreter::{GasTracker, Host, InstrStop, Interpreter, Message, MessageResult},
    registry::{HandlerError, HandlerResult, TxRequest},
};

use crate::{BaseEvmTypes, BaseTxEnvelope, BaseTxResultExt, NonceManager};

/// Address of the transaction-context precompile (`TX_CONTEXT_ADDRESS`, EIP-8430). The executor
/// publishes the sender, payer and sender actor id in its transient storage slots 0, 1 and 2.
pub const TX_CONTEXT_ADDRESS: Address = address!("813000000000000000000000000000000000aa02");

/// EIP-3529 maximum refund quotient.
const MAX_REFUND_QUOTIENT: u64 = 5;

/// EIP-2929 cold account access cost.
const COLD_ACCOUNT_ACCESS_COST: u64 = 2_600;

/// EIP-2929 warm account access cost.
const WARM_ACCOUNT_ACCESS_COST: u64 = 100;

/// Bisection steps of the [`TxBaseSimulation::Estimate`] gas search before it returns the
/// tightest verified-feasible pool.
const POOL_SEARCH_MAX_ITERS: u32 = 16;

/// Early-exit tolerance of the gas search, in parts per thousand of the feasible bound.
const POOL_SEARCH_TOLERANCE_PER_MILLE: u64 = 15;

/// Slot of the `BaseTime` predeploy that packs the block's millisecond part.
const BASE_TIME_MILLIS_SLOT: U256 = U256::ZERO;

/// Why a Base transaction is invalid. Every variant makes the transaction non-includable: a
/// block that carries it is invalid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxBaseError {
    /// The transaction's chain id is not the local chain's.
    ChainIdMismatch,
    /// The block timestamp does not fit in `u64`.
    TimestampOverflow,
    /// The block is before the transaction's `valid_after`.
    NotYetValid,
    /// The block is past the transaction's `valid_before`.
    Expired,
    /// The bare `sender_auth` does not recover a signer.
    SenderRecovery,
    /// An open payer's `payer_auth` does not recover a signer.
    PayerRecovery,
    /// A self-paid transaction carries a `payer_auth`.
    UnexpectedPayerAuth,
    /// A named sender or payer selects an authenticator other than native secp256k1.
    UnsupportedAuthenticator,
    /// A named sender's or payer's signature is malformed or does not recover.
    SignatureRecovery,
    /// A named sender's or payer's signature recovers to another account.
    SignerMismatch,
    /// More than one account change.
    MultipleDelegations,
    /// A delegation would replace ordinary contract code.
    NonDelegatableCode,
    /// A delegation on a transaction that does not use the protocol nonce.
    DelegationRequiresProtocolNonce,
    /// The nonce sequence is below the channel nonce.
    NonceTooLow {
        /// The channel's current nonce.
        channel: u64,
        /// The transaction's sequence.
        got: u64,
    },
    /// The nonce sequence is above the channel nonce.
    NonceTooHigh {
        /// The channel's current nonce.
        channel: u64,
        /// The transaction's sequence.
        got: u64,
    },
    /// The nonce sequence is `u64::MAX` and cannot be consumed.
    NonceSequenceSaturated,
    /// A nonce-free transaction carries a non-zero sequence.
    NonceFreeSequence,
    /// A nonce-free transaction's replay id is recorded and unexpired.
    NonceFreeReplay,
    /// The gas limit is below the EIP-7623 sender floor.
    GasBelowFloor,
    /// The gas limit is below sender-intrinsic gas.
    GasBelowIntrinsic,
    /// The gas limit plus payer authentication exceeds the EIP-7825 per-transaction cap.
    GasAboveCap,
    /// `max_priority_fee_per_gas` exceeds `max_fee_per_gas`.
    TipAboveFeeCap,
    /// `max_fee_per_gas` is below the block base fee.
    FeeCapBelowBaseFee,
    /// A settlement amount overflowed `U256`.
    FeeOverflow,
    /// A Base transaction reached the simulation path before Everest.
    NotActive,
}

impl core::fmt::Display for TxBaseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ChainIdMismatch => f.write_str("chain id mismatch"),
            Self::TimestampOverflow => f.write_str("block timestamp exceeds u64"),
            Self::NotYetValid => f.write_str("transaction is not yet valid"),
            Self::Expired => f.write_str("transaction validity window has expired"),
            Self::SenderRecovery => f.write_str("EOA sender signature could not be recovered"),
            Self::PayerRecovery => f.write_str("open payer signature could not be recovered"),
            Self::UnexpectedPayerAuth => f.write_str("self-paid transaction carries a payer_auth"),
            Self::UnsupportedAuthenticator => f.write_str("unsupported authenticator"),
            Self::SignatureRecovery => f.write_str("secp256k1 signature could not be recovered"),
            Self::SignerMismatch => {
                f.write_str("secp256k1 signature does not recover to the named account")
            }
            Self::MultipleDelegations => f.write_str("at most one delegation entry is allowed"),
            Self::NonDelegatableCode => {
                f.write_str("delegation cannot replace non-delegation code")
            }
            Self::DelegationRequiresProtocolNonce => {
                f.write_str("a transaction carrying a delegation must use the protocol nonce")
            }
            Self::NonceTooLow { channel, got } => {
                write!(f, "nonce sequence {got} is below the channel nonce {channel}")
            }
            Self::NonceTooHigh { channel, got } => {
                write!(f, "nonce sequence {got} is ahead of the channel nonce {channel}")
            }
            Self::NonceSequenceSaturated => {
                f.write_str("nonce sequence is u64::MAX and cannot be consumed")
            }
            Self::NonceFreeSequence => f.write_str("nonce-free transaction has a nonce sequence"),
            Self::NonceFreeReplay => f.write_str("nonce-free replay hash already recorded"),
            Self::GasBelowFloor => {
                f.write_str("Base transaction gas limit is below the EIP-7623 calldata floor")
            }
            Self::GasBelowIntrinsic => {
                f.write_str("Base transaction gas limit is below intrinsic gas")
            }
            Self::GasAboveCap => {
                f.write_str("transaction gas exceeds the EIP-7825 per-transaction cap")
            }
            Self::TipAboveFeeCap => f.write_str("max_priority_fee_per_gas exceeds max_fee_per_gas"),
            Self::FeeCapBelowBaseFee => f.write_str("max_fee_per_gas is below the base fee"),
            Self::FeeOverflow => f.write_str("Base transaction fee overflow"),
            Self::NotActive => f.write_str("Base transactions are not active before Everest"),
        }
    }
}

impl core::error::Error for TxBaseError {}

/// Which read-only RPC simulation [`TxBaseExecutor::simulate`] runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxBaseSimulation {
    /// `eth_call`: the calls run once at the transaction's gas limit; the reported gas is that
    /// run's metered gas (gross of refunds, EIP-7623 floor applied, payer authentication
    /// included).
    Call,
    /// `eth_estimateGas`: the reported gas is the smallest gas limit (found by search) at which
    /// the calls succeed. Under gas-limit billing it excludes payer authentication, which
    /// settlement bills on top of the declared limit.
    Estimate,
}

/// The resolved pre-call context of a Base transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Outcome {
    sender: Address,
    payer: Address,
    /// The sender-signed gas budget.
    gas_limit: u64,
    /// Intrinsic gas excluding payer authentication.
    sender_intrinsic: u64,
    /// EIP-7623 sender floor; `gas_limit >= sender_floor`.
    sender_floor: u64,
    /// Payer authentication, billed on top of `gas_limit`.
    payer_auth_cost: u64,
    /// Gas available to the calls: `gas_limit - sender_intrinsic`.
    execution_gas_available: u64,
    /// EIP-1559 effective gas price.
    effective: u128,
    base_fee: u128,
    /// Whether the protocol (account) nonce is consumed (`nonce_key == 0`).
    bump_protocol_nonce: bool,
}

impl Outcome {
    const fn floored_sender_gas(&self, sender_gas: u64) -> u64 {
        if sender_gas < self.sender_floor { self.sender_floor } else { sender_gas }
    }

    /// `max(sender_gas, sender_floor) + payer_auth_cost`.
    const fn metered_gas(&self, sender_gas: u64) -> u64 {
        self.floored_sender_gas(sender_gas).saturating_add(self.payer_auth_cost)
    }

    /// The reserved block gas: `gas_limit + payer_auth_cost`.
    const fn max_chargeable_gas(&self) -> u64 {
        self.gas_limit.saturating_add(self.payer_auth_cost)
    }

    /// Metered gas net of the EIP-3529-capped refund (the pre-gas-limit-billing charge).
    fn metered_gas_used(&self, calls: &CallsResult) -> u64 {
        let gross_used = self.sender_intrinsic.saturating_add(calls.call_gas_spent);
        let refund =
            u64::try_from(calls.refund.max(0)).unwrap_or(0).min(gross_used / MAX_REFUND_QUOTIENT);
        self.metered_gas(gross_used - refund)
    }
}

/// The result of dispatching a Base transaction's calls.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CallsResult {
    /// Gas drawn from the pool, across all phases (including a reverting phase).
    call_gas_spent: u64,
    /// Signed refund counter summed over the committed phases, before the EIP-3529 cap.
    refund: i64,
    /// Whether a phase reverted (later phases are skipped).
    reverted: bool,
    /// The reverting call's output, or the last successful call's output.
    output: Bytes,
    /// Number of leading phases that committed.
    committed_phases: u64,
}

/// Executes enshrined Base transactions on an evm2 [`Evm`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TxBaseExecutor;

impl TxBaseExecutor {
    /// Payer published to the transaction context when simulating an open-payer transaction
    /// whose `payer_auth` does not recover yet (no payer has signed).
    pub const UNSIGNED_OPEN_PAYER: Address = address!("0x0000000000000000000000000000000000008130");

    /// Executes the Base transaction `signed`, whose EIP-2718 encoding is `enveloped`, on `evm`'s
    /// transaction overlay. The registry handler for type `0x79`.
    ///
    /// Returns a result for an included transaction whether or not its calls reverted, with
    /// `status` false for a phase revert and [`BaseTxResultExt::committed_phases`] set. Returns
    /// an error when the transaction is invalid; evm2 then discards the overlay.
    pub fn execute(
        evm: &mut Evm<'_, BaseEvmTypes>,
        signed: &TxBaseSigned,
        enveloped: &Bytes,
    ) -> HandlerResult<TxResult<BaseEvmTypes>> {
        let tx = signed.tx();
        assert!(
            evm.config_spec_id().is_feature_enabled(BaseFeature::TxBase),
            "the 0x79 handler is registered only from Everest"
        );
        assert_eq!(enveloped.first(), Some(&TxBaseConstants::BASE_TX_TYPE));

        // Consensus-critical cross-chain replay guard: signatures commit to the embedded chain id,
        // and block inclusion bypasses the pool's check.
        if tx.chain_id != evm.version().chain_id {
            return Err(HandlerError::external(TxBaseError::ChainIdMismatch));
        }
        let now = u64::try_from(evm.block().timestamp)
            .map_err(|_| HandlerError::external(TxBaseError::TimestampOverflow))?;
        // Everest implies Denim, so the block's first deposit has already written this block's
        // millisecond part to `BaseTime`. Read untracked, so the slot is not warmed.
        // The packed `uint16` is the low 16 bits of the slot word.
        let millis = evm
            .state_mut()
            .storage_slot_untracked(&Predeploys::BASE_TIME, &BASE_TIME_MILLIS_SLOT)
            .map_err(HandlerError::Fatal)?
            .as_limbs()[0] as u16;
        let now_ms = now.wrapping_mul(1_000).wrapping_add(u64::from(millis));
        let base_fee = evm.block().basefee.saturating_to::<u128>();
        let fee_charge = evm.feature(EvmFeatures::FEE_CHARGE);

        let outcome = Self::authorize_and_apply(evm, signed, now_ms, base_fee, fee_charge)?;
        Self::consume_sender_nonce(evm, &outcome)?;
        let prepay = if fee_charge { Self::prepay(evm, &outcome, enveloped)? } else { U256::ZERO };
        Self::warm_pre_call_accounts(evm, &outcome)?;

        let root = Self::open_trace_root(evm, &outcome, || enveloped.clone());
        let calls = Self::execute_calls(evm, tx, &outcome, outcome.execution_gas_available)
            .inspect_err(|_| Self::close_trace_root(evm, root.as_ref(), None, &outcome, 0))?;
        let (gas_used, l1_fee) = if fee_charge {
            Self::settle_fees(evm, &outcome, &calls, prepay, enveloped).inspect_err(|_| {
                Self::close_trace_root(
                    evm,
                    root.as_ref(),
                    Some(&calls),
                    &outcome,
                    outcome.gas_limit,
                )
            })?
        } else {
            (outcome.metered_gas_used(&calls), U256::ZERO)
        };
        Self::close_trace_root(evm, root.as_ref(), Some(&calls), &outcome, gas_used);
        assert!(calls.committed_phases <= tx.calls.len() as u64);
        assert_eq!(calls.reverted, calls.committed_phases < tx.calls.len() as u64);

        Ok(TxResult::<BaseEvmTypes> {
            status: !calls.reverted,
            total_gas_spent: gas_used,
            stop: if calls.reverted { InstrStop::Revert } else { InstrStop::Return },
            output: calls.output,
            ext: BaseTxResultExt { l1_fee, committed_phases: calls.committed_phases },
            ..Default::default()
        })
    }

    /// Read-only gas measurement of the Base transaction `signed`, simulated as sent from `from`
    /// (the `eth_call` / `eth_estimateGas` path).
    ///
    /// Runs the account-change, intrinsic-gas and phased-calls pipeline without verifying any
    /// signature (the auth blobs must still have an accepted shape), without validating the nonce
    /// or the validity window, and without charging fees. The result's `tx_gas_used()` is the
    /// reported gas (see [`TxBaseSimulation`]); its logs and output come from the final run.
    /// The transaction overlay is left holding the simulated writes: the caller discards it
    /// (`evm.state_mut().clear_transaction_state()`), as it does after any simulation.
    pub fn simulate(
        evm: &mut Evm<'_, BaseEvmTypes>,
        signed: &TxBaseSigned,
        from: Address,
        mode: TxBaseSimulation,
    ) -> HandlerResult<TxResult<BaseEvmTypes>> {
        if !evm.config_spec_id().is_feature_enabled(BaseFeature::TxBase) {
            return Err(HandlerError::external(TxBaseError::NotActive));
        }
        let tx = signed.tx();
        let base_fee = evm.block().basefee.saturating_to::<u128>();
        let outcome = Self::simulate_resolve(evm, signed, from, base_fee)?;
        Self::consume_sender_nonce(evm, &outcome)?;
        Self::warm_pre_call_accounts(evm, &outcome)?;

        let root = Self::open_trace_root(evm, &outcome, || signed.encoded_2718().into());
        let (calls, gas) = Self::simulate_calls(evm, tx, &outcome, mode)
            .inspect_err(|_| Self::close_trace_root(evm, root.as_ref(), None, &outcome, 0))?;
        Self::close_trace_root(evm, root.as_ref(), Some(&calls), &outcome, gas);
        Ok(TxResult::<BaseEvmTypes> {
            status: !calls.reverted,
            total_gas_spent: gas,
            stop: if calls.reverted { InstrStop::Revert } else { InstrStop::Return },
            output: calls.output,
            logs: evm.state().logs().to_vec(),
            ext: BaseTxResultExt { l1_fee: U256::ZERO, committed_phases: calls.committed_phases },
            ..Default::default()
        })
    }

    /// Runs the simulation's calls for `mode` once more after any estimate search, and returns
    /// that final run with the gas the simulation reports.
    fn simulate_calls(
        evm: &mut Evm<'_, BaseEvmTypes>,
        tx: &TxBase,
        outcome: &Outcome,
        mode: TxBaseSimulation,
    ) -> HandlerResult<(CallsResult, u64)> {
        let ceiling_pool = outcome.execution_gas_available;
        let pool = match mode {
            TxBaseSimulation::Call => None,
            TxBaseSimulation::Estimate => {
                // The search's probes are not user-visible executions: detach the inspector so a
                // trace records only the final run.
                let inspector = evm.clear_inspector();
                let searched = Self::estimate_pool(evm, tx, outcome, ceiling_pool);
                if let Some(inspector) = inspector {
                    evm.set_boxed_inspector(inspector);
                }
                searched?
            }
        };
        let calls = Self::execute_calls(evm, tx, outcome, pool.unwrap_or(ceiling_pool))?;
        let Some(pool) = pool else {
            let gas =
                outcome.metered_gas(outcome.sender_intrinsic.saturating_add(calls.call_gas_spent));
            return Ok((calls, gas));
        };
        let sender_budget =
            outcome.floored_sender_gas(outcome.sender_intrinsic.saturating_add(pool));
        let gas = if evm.config_spec_id().is_feature_enabled(BaseFeature::GasLimitBilling) {
            sender_budget
        } else {
            sender_budget.saturating_add(outcome.payer_auth_cost)
        };
        Ok((calls, gas))
    }

    /// The estimate's call pool, or `None` when the calls fail even at `ceiling_pool` (a genuine
    /// failure the search cannot fix, reported from one run at the full pool). Refunds never
    /// rejoin the pool and EIP-150 forwarding keeps 1/64 per level, so the pool that succeeds
    /// can exceed one run's spend: search the minimum.
    fn estimate_pool(
        evm: &mut Evm<'_, BaseEvmTypes>,
        tx: &TxBase,
        outcome: &Outcome,
        ceiling_pool: u64,
    ) -> HandlerResult<Option<u64>> {
        let ceiling = Self::probe_calls(evm, tx, outcome, ceiling_pool)?;
        if ceiling.reverted {
            return Ok(None);
        }
        Self::search_estimate_pool(evm, tx, outcome, ceiling.call_gas_spent, ceiling_pool).map(Some)
    }

    /// Runs the calls at `pool` and rolls back every write and log they made.
    fn probe_calls(
        evm: &mut Evm<'_, BaseEvmTypes>,
        tx: &TxBase,
        outcome: &Outcome,
        pool: u64,
    ) -> HandlerResult<CallsResult> {
        let checkpoint = evm.state().checkpoint();
        let calls = Self::execute_calls(evm, tx, outcome, pool)?;
        let features = evm.version().features;
        evm.state_mut().rollback(checkpoint, features);
        Ok(calls)
    }

    /// The minimum pool in `[ceiling_spent, ceiling_pool]` at which the calls succeed, given
    /// they succeeded at `ceiling_pool` spending `ceiling_spent`. Every value assigned to
    /// `highest` is probe-verified.
    fn search_estimate_pool(
        evm: &mut Evm<'_, BaseEvmTypes>,
        tx: &TxBase,
        outcome: &Outcome,
        ceiling_spent: u64,
        ceiling_pool: u64,
    ) -> HandlerResult<u64> {
        let spent = ceiling_spent.min(ceiling_pool);
        if spent >= ceiling_pool || !Self::probe_calls(evm, tx, outcome, spent)?.reverted {
            return Ok(spent);
        }
        let mut lowest = spent;
        let mut highest = ceiling_pool;
        let seed = spent.saturating_mul(64) / 63;
        if seed > lowest && seed < highest {
            if Self::probe_calls(evm, tx, outcome, seed)?.reverted {
                lowest = seed;
            } else {
                highest = seed;
            }
        }
        let mut iters = 0;
        while lowest + 1 < highest && iters < POOL_SEARCH_MAX_ITERS {
            if (highest - lowest).saturating_mul(1000)
                <= highest.saturating_mul(POOL_SEARCH_TOLERANCE_PER_MILLE)
            {
                break;
            }
            let mid = lowest + (highest - lowest) / 2;
            if Self::probe_calls(evm, tx, outcome, mid)?.reverted {
                lowest = mid;
            } else {
                highest = mid;
            }
            iters += 1;
        }
        Ok(highest)
    }

    /// The simulation pre-call pipeline: auth shape check, intrinsic gas, delegation install,
    /// 2D-channel advance (unvalidated) and transaction context. No signature is recovered
    /// except an open payer's (to publish the payer a real execution would see).
    fn simulate_resolve(
        evm: &mut Evm<'_, BaseEvmTypes>,
        signed: &TxBaseSigned,
        sender: Address,
        base_fee: u128,
    ) -> HandlerResult<Outcome> {
        let tx = signed.tx();
        let payer_auth_cost = IntrinsicGas::payer_auth_cost_of(signed);
        let gas_limit =
            tx.gas_limit.min(evm.version().tx_gas_limit_cap.saturating_sub(payer_auth_cost));
        let payer = signed.resolved_payer(sender).unwrap_or(Self::UNSIGNED_OPEN_PAYER);
        Self::check_auth_shape(signed)?;
        let outcome = Self::outcome(signed, sender, payer, gas_limit, base_fee, tx.nonce_key.is_zero())?;
        if let Some(target) = Self::delegation(signed)? {
            Self::install_delegation(evm, sender, target)?;
        }
        if !tx.nonce_key.is_zero() && tx.nonce_key != TxBaseConstants::NONCE_KEY_MAX {
            let _ = NonceManager::increment_nonce(evm, sender, tx.nonce_key)?;
        }
        Self::set_tx_context(evm, sender, payer);
        Ok(outcome)
    }

    /// The execution pre-call pipeline, in the reference order: validity window, nonce check,
    /// authentication, delegation install, nonce advance, intrinsic gas, fee caps and payer
    /// balance, transaction context.
    fn authorize_and_apply(
        evm: &mut Evm<'_, BaseEvmTypes>,
        signed: &TxBaseSigned,
        now_ms: u64,
        base_fee: u128,
        fee_charge: bool,
    ) -> HandlerResult<Outcome> {
        let tx = signed.tx();
        let nonce_key = tx.nonce_key;

        // Validity window on the millisecond axis (`0` disables a bound).
        let valid_after = tx.valid_after_ms();
        if valid_after != 0 && now_ms < valid_after {
            return Err(HandlerError::external(TxBaseError::NotYetValid));
        }
        if tx.is_expired_at(now_ms) {
            return Err(HandlerError::external(TxBaseError::Expired));
        }

        // 1. Resolve the sender and validate the nonce against the pre-transaction state.
        let recovered = tx.sender.is_none();
        let sender = signed.recover_sender().map_err(|_| HandlerError::external(TxBaseError::SenderRecovery))?;
        let protocol_nonce = {
            let mut account =
                evm.state_mut().account(&sender, false).map_err(HandlerError::Fatal)?;
            let _ = account.warm();
            account.nonce()
        };
        Self::validate_nonce(evm, tx, sender, protocol_nonce, now_ms)?;

        // 2. Authenticate the sender and payer (reads no state).
        Self::check_auth_shape(signed)?;
        if !recovered {
            Self::recover_named_k1(sender, tx.sender_signature_hash(), signed.sender_auth())?;
        }
        let payer = match tx.payer {
            Some(account) if !tx.is_open_payer() => {
                Self::recover_named_k1(account, tx.payer_signature_hash(sender), signed.payer_auth())?
            }
            _ => signed.resolved_payer(sender).map_err(|_| HandlerError::external(TxBaseError::PayerRecovery))?,
        };
        let delegation = Self::delegation(signed)?;

        // 3. Install the delegation indicator.
        if let Some(target) = delegation {
            Self::install_delegation(evm, sender, target)?;
        }

        // 4. Advance the nonce validated in step 1 (the protocol nonce is bumped later).
        let bump_protocol_nonce = if nonce_key == TxBaseConstants::NONCE_KEY_MAX {
            NonceManager::check_and_mark_expiring_nonce(
                evm,
                tx.replay_id(sender),
                tx.valid_before_ms(),
                now_ms,
            )?;
            false
        } else if nonce_key.is_zero() {
            true
        } else {
            let _ = NonceManager::increment_nonce(evm, sender, nonce_key)?;
            false
        };

        // 5. Intrinsic gas and the EIP-7825 cap on `gas_limit + payer_auth_cost`.
        let outcome = Self::outcome(signed, sender, payer, tx.gas_limit, base_fee, bump_protocol_nonce)?;
        if outcome.max_chargeable_gas() > evm.version().tx_gas_limit_cap {
            return Err(HandlerError::external(TxBaseError::GasAboveCap));
        }

        // 6. Fee caps and, when fees are charged, the payer's worst-case balance.
        if tx.max_priority_fee_per_gas > tx.max_fee_per_gas {
            return Err(HandlerError::external(TxBaseError::TipAboveFeeCap));
        }
        if tx.max_fee_per_gas < base_fee {
            return Err(HandlerError::external(TxBaseError::FeeCapBelowBaseFee));
        }
        if fee_charge {
            let balance = {
                let mut account =
                    evm.state_mut().account(&payer, false).map_err(HandlerError::Fatal)?;
                let _ = account.warm();
                account.balance()
            };
            let required = U256::from(outcome.max_chargeable_gas()).saturating_mul(U256::from(tx.max_fee_per_gas));
            if balance < required {
                return Err(HandlerError::InsufficientFunds);
            }
        }

        // 7. Publish the transaction context.
        Self::set_tx_context(evm, sender, payer);
        Ok(outcome)
    }

    /// Validates the 2D nonce for block inclusion (the reference `NonceValidator` in
    /// `NonceMode::Inclusion`). Channel and replay reads warm their slots.
    fn validate_nonce(
        evm: &mut Evm<'_, BaseEvmTypes>,
        tx: &TxBase,
        sender: Address,
        protocol_nonce: u64,
        now_ms: u64,
    ) -> HandlerResult<()> {
        if !tx.nonce_key.is_zero()
            && tx
                .account_changes
                .iter()
                .any(|change| matches!(change, AccountChange::Delegation(_)))
        {
            return Err(HandlerError::external(TxBaseError::DelegationRequiresProtocolNonce));
        }
        if tx.nonce_key == TxBaseConstants::NONCE_KEY_MAX {
            if tx.nonce_sequence != 0 {
                return Err(HandlerError::external(TxBaseError::NonceFreeSequence));
            }
            if NonceManager::expiring_nonce_seen(evm, tx.replay_id(sender), now_ms)? {
                return Err(HandlerError::external(TxBaseError::NonceFreeReplay));
            }
            return Ok(());
        }
        let channel = if tx.nonce_key.is_zero() {
            protocol_nonce
        } else {
            NonceManager::channel_nonce(evm, sender, tx.nonce_key)?
        };
        let got = tx.nonce_sequence;
        if got == u64::MAX {
            return Err(HandlerError::external(TxBaseError::NonceSequenceSaturated));
        }
        if got < channel {
            return Err(HandlerError::external(TxBaseError::NonceTooLow { channel, got }));
        }
        if got > channel {
            return Err(HandlerError::external(TxBaseError::NonceTooHigh { channel, got }));
        }
        Ok(())
    }

    /// Checks the auth shape without verifying signatures: a self-paid transaction carries no
    /// `payer_auth`, and a named sender or payer selects the native secp256k1 authenticator.
    fn check_auth_shape(signed: &TxBaseSigned) -> HandlerResult<()> {
        let tx = signed.tx();
        if tx.payer.is_none() && !signed.payer_auth().is_empty() {
            return Err(HandlerError::external(TxBaseError::UnexpectedPayerAuth));
        }
        let named_k1 = |auth: &[u8]| auth.starts_with(TxBaseConstants::K1_AUTHENTICATOR.as_slice());
        if (tx.sender.is_some() && !named_k1(signed.sender_auth()))
            || (tx.payer.is_some() && !tx.is_open_payer() && !named_k1(signed.payer_auth()))
        {
            return Err(HandlerError::external(TxBaseError::UnsupportedAuthenticator));
        }
        Ok(())
    }

    /// Recovers `auth` (`K1_AUTHENTICATOR || r || s || v`) over `hash` and requires `account`.
    fn recover_named_k1(account: Address, hash: B256, auth: &[u8]) -> HandlerResult<Address> {
        let signature = auth
            .strip_prefix(TxBaseConstants::K1_AUTHENTICATOR.as_slice())
            .ok_or(HandlerError::external(TxBaseError::UnsupportedAuthenticator))?;
        let recovered = TxBaseSigned::recover_raw_k1(hash, signature)
            .map_err(|_| HandlerError::external(TxBaseError::SignatureRecovery))?;
        if recovered != account {
            return Err(HandlerError::external(TxBaseError::SignerMismatch));
        }
        Ok(account)
    }

    /// The delegation target of the transaction's account change, if it carries one.
    fn delegation(signed: &TxBaseSigned) -> HandlerResult<Option<Address>> {
        let changes = &signed.tx().account_changes;
        if changes.len() > TxBaseConstants::MAX_ACCOUNT_CHANGES_PER_TX {
            return Err(HandlerError::external(TxBaseError::MultipleDelegations));
        }
        Ok(changes.first().map(|AccountChange::Delegation(Delegation { target })| *target))
    }

    /// Sets (or, for a zero `target`, clears) the sender's EIP-7702 delegation indicator. Only
    /// empty code or code starting with the indicator prefix may be replaced.
    fn install_delegation(
        evm: &mut Evm<'_, BaseEvmTypes>,
        sender: Address,
        target: Address,
    ) -> HandlerResult<()> {
        let mut account = evm.state_mut().account(&sender, false).map_err(HandlerError::Fatal)?;
        let _ = account.warm();
        let code = account.load_code().map_err(HandlerError::Fatal)?;
        let bytes = code.original_bytes();
        if !bytes.is_empty() && !bytes.starts_with(&TxBaseConstants::DELEGATION_INDICATOR_PREFIX) {
            return Err(HandlerError::external(TxBaseError::NonDelegatableCode));
        }
        let code =
            if target.is_zero() { Bytecode::default() } else { Bytecode::new_eip7702(target) };
        account.set_code_slow(code);
        Ok(())
    }

    /// Writes the sender, payer and sender actor id to the transaction-context precompile's
    /// transient storage. All three are non-zero, so a zero slot keeps meaning "unset".
    fn set_tx_context(evm: &mut Evm<'_, BaseEvmTypes>, sender: Address, payer: Address) {
        assert!(!sender.is_zero() && !payer.is_zero(), "Base tx sender and payer are non-zero");
        let sender_word = U256::from_be_bytes(sender.into_word().0);
        let payer_word = U256::from_be_bytes(payer.into_word().0);
        let state = evm.state_mut();
        state.tstore(&TX_CONTEXT_ADDRESS, &U256::ZERO, &sender_word);
        state.tstore(&TX_CONTEXT_ADDRESS, &U256::from(1), &payer_word);
        // The sender actor id is the sender address right-aligned in a word.
        state.tstore(&TX_CONTEXT_ADDRESS, &U256::from(2), &sender_word);
    }

    /// Bumps the sender's protocol nonce when the transaction uses it, and makes the sender
    /// `tx.origin` for native precompiles.
    fn consume_sender_nonce(
        evm: &mut Evm<'_, BaseEvmTypes>,
        outcome: &Outcome,
    ) -> HandlerResult<()> {
        evm.ext_mut().tx_origin = outcome.sender;
        if outcome.bump_protocol_nonce {
            let bumped = evm
                .state_mut()
                .account(&outcome.sender, false)
                .map_err(HandlerError::Fatal)?
                .bump_nonce();
            assert!(bumped, "nonce validation rejects a u64::MAX protocol nonce");
        }
        Ok(())
    }

    /// Pre-charges the payer the worst case: `(gas_limit + payer_auth_cost) * effective` plus the
    /// L1 data fee and the operator fee on that gas. Returns the amount debited.
    fn prepay(
        evm: &mut Evm<'_, BaseEvmTypes>,
        outcome: &Outcome,
        enveloped: &Bytes,
    ) -> HandlerResult<U256> {
        let max_gas = outcome.max_chargeable_gas();
        let upgrade = evm.config_spec_id().upgrade();
        let gas_charge = U256::from(max_gas)
            .checked_mul(U256::from(outcome.effective))
            .ok_or(HandlerError::external(TxBaseError::FeeOverflow))?;
        let l1_cost = evm.ext().l1.calculate_tx_l1_cost(enveloped, upgrade);
        let operator_cost =
            evm.ext().l1.operator_fee_charge(enveloped, U256::from(max_gas), upgrade);
        let prepay = gas_charge
            .checked_add(l1_cost)
            .and_then(|value| value.checked_add(operator_cost))
            .ok_or(HandlerError::external(TxBaseError::FeeOverflow))?;
        let mut payer =
            evm.state_mut().account(&outcome.payer, false).map_err(HandlerError::Fatal)?;
        let balance = payer.balance().checked_sub(prepay).ok_or(HandlerError::InsufficientFunds)?;
        payer.set_balance(balance);
        Ok(prepay)
    }

    /// Warms what a normal transaction starts with: the precompiles, the coinbase (EIP-3651),
    /// the sender and the payer.
    fn warm_pre_call_accounts(
        evm: &mut Evm<'_, BaseEvmTypes>,
        outcome: &Outcome,
    ) -> HandlerResult<()> {
        evm.warm_precompiles();
        if evm.feature(EvmFeatures::EIP3651) {
            let coinbase = evm.block().beneficiary;
            evm.state_mut().prewarm(&coinbase);
        }
        for address in [outcome.sender, outcome.payer] {
            let _ = evm.state_mut().account(&address, false).map_err(HandlerError::Fatal)?.warm();
        }
        Ok(())
    }

    /// Dispatches the calls phase by phase from one gas `pool`. A phase runs under a state
    /// checkpoint: it commits if every call succeeds, otherwise it rolls back and the remaining
    /// phases are skipped.
    fn execute_calls(
        evm: &mut Evm<'_, BaseEvmTypes>,
        tx: &TxBase,
        outcome: &Outcome,
        pool: u64,
    ) -> HandlerResult<CallsResult> {
        let tx_env = Self::tx_env(evm, outcome);
        let mut remaining = pool;
        let mut refund: i64 = 0;
        let mut committed_phases: u64 = 0;
        let mut last_output = Bytes::new();
        let mut first_call = true;

        for phase in &tx.calls {
            let checkpoint = evm.state().checkpoint();
            let mut phase_refund: i64 = 0;
            let mut reverted_output = None;

            for call in phase {
                let access_gas = Self::root_call_access_gas(evm, call.to, first_call)?;
                first_call = false;
                let Some(left) = remaining.checked_sub(access_gas) else {
                    reverted_output = Some(Bytes::new());
                    break;
                };
                remaining = left;
                if Self::creates_account(evm, outcome.sender, call.to, call.value)? {
                    // An unaffordable charge leaves `remaining` as is: only consumed gas bills.
                    let Some(left) = remaining.checked_sub(TxBaseGasSchedule::NEW_ACCOUNT_COST)
                    else {
                        reverted_output = Some(Bytes::new());
                        break;
                    };
                    remaining = left;
                }

                let mut message = Self::call_message(
                    evm,
                    outcome.sender,
                    call.to,
                    call.value,
                    call.data.clone(),
                    remaining,
                )?;
                let result = evm.execute_message(&tx_env, &mut message);
                if let Some(code) = evm.error_code() {
                    return Err(HandlerError::Fatal(code));
                }
                // A halt has already zeroed the frame's remaining gas.
                let spent = remaining - result.gas.remaining().min(remaining);
                assert!(result.gas.remaining() <= remaining, "a call cannot gain gas");
                remaining -= spent;
                if result.stop.is_success() {
                    phase_refund = phase_refund.saturating_add(result.gas.refunded());
                    last_output = result.output;
                } else {
                    reverted_output = Some(result.output);
                    break;
                }
            }

            if let Some(output) = reverted_output {
                let features = evm.version().features;
                evm.state_mut().rollback(checkpoint, features);
                return Ok(CallsResult {
                    call_gas_spent: pool - remaining,
                    refund,
                    reverted: true,
                    output,
                    committed_phases,
                });
            }
            committed_phases += 1;
            refund = refund.saturating_add(phase_refund);
        }

        Ok(CallsResult {
            call_gas_spent: pool - remaining,
            refund,
            reverted: false,
            output: last_output,
            committed_phases,
        })
    }

    /// EIP-2929 access gas for dispatching a root call to `to`: cold or warm access for `to`
    /// (skipped for the first call, whose target access is intrinsic), plus the same for an
    /// EIP-7702 delegation target. Warms both.
    fn root_call_access_gas(
        evm: &mut Evm<'_, BaseEvmTypes>,
        to: Address,
        target_prepaid: bool,
    ) -> HandlerResult<u64> {
        let access_cost = |was_cold: bool| {
            if was_cold { COLD_ACCOUNT_ACCESS_COST } else { WARM_ACCOUNT_ACCESS_COST }
        };
        let (target_cold, code) = {
            let mut account = evm.state_mut().account(&to, false).map_err(HandlerError::Fatal)?;
            let was_cold = account.warm();
            (was_cold, account.load_code().map_err(HandlerError::Fatal)?)
        };
        let mut gas = if target_prepaid { 0 } else { access_cost(target_cold) };
        if let Some(delegate) = code.eip7702_address() {
            let was_cold =
                evm.state_mut().account(&delegate, false).map_err(HandlerError::Fatal)?.warm();
            gas += access_cost(was_cold);
        }
        Ok(gas)
    }

    /// Whether moving `value` from `sender` to `to` creates `to`: non-zero value, an empty
    /// (EIP-161) target that is not the sender, and a sender that can cover the value.
    fn creates_account(
        evm: &mut Evm<'_, BaseEvmTypes>,
        sender: Address,
        to: Address,
        value: U256,
    ) -> HandlerResult<bool> {
        if value.is_zero() || to == sender {
            return Ok(false);
        }
        let empty = evm
            .state_mut()
            .account(&to, false)
            .map_err(HandlerError::Fatal)?
            .get()
            .is_none_or(|info| info.is_empty());
        if !empty {
            return Ok(false);
        }
        let balance =
            evm.state_mut().account(&sender, false).map_err(HandlerError::Fatal)?.balance();
        Ok(balance >= value)
    }

    /// The transaction environment the calls run in: the resolved sender is `tx.origin`.
    fn tx_env(evm: &Evm<'_, BaseEvmTypes>, outcome: &Outcome) -> TxEnv<BaseEvmTypes> {
        TxEnvExt {
            origin: outcome.sender,
            gas_price: U256::from(outcome.effective),
            chain_id: U256::from(evm.version().chain_id),
            ..TxEnvExt::default()
        }
    }

    /// Opens the synthetic trace root when an inspector is installed: one call frame from the
    /// sender to itself with the enveloped transaction as input and the sender's gas limit, so
    /// the root calls the executor dispatches attach to one connected call tree, as in the
    /// reference (`start_inspection`). Returns the root message to close with
    /// [`Self::close_trace_root`]; `None` without an inspector, so `enveloped` is not built.
    fn open_trace_root(
        evm: &mut Evm<'_, BaseEvmTypes>,
        outcome: &Outcome,
        enveloped: impl FnOnce() -> Bytes,
    ) -> Option<Message<BaseEvmTypes>> {
        evm.inspector()?;
        let mut message = Message::<BaseEvmTypes> {
            gas_limit: outcome.gas_limit,
            destination: outcome.sender,
            caller: outcome.sender,
            input: enveloped(),
            code_address: outcome.sender,
            ..Default::default()
        };
        // The root runs no code, so a hook's override result is ignored, as in the reference.
        Self::with_root_frame(evm, outcome, &message.clone(), |inspector, frame| {
            let _ = inspector.call(frame, &mut message);
        });
        Some(message)
    }

    /// Closes the synthetic trace root opened by [`Self::open_trace_root`] (a no-op for `None`).
    /// `calls` is the final run, or `None` when execution failed, which closes the root as a
    /// revert with all of its gas spent. The root reports `gas_used`, the gas the receipt or
    /// simulation reports, widened past the sender's limit when gas-limit billing adds payer
    /// authentication on top (the reference `end_inspection`).
    fn close_trace_root(
        evm: &mut Evm<'_, BaseEvmTypes>,
        root: Option<&Message<BaseEvmTypes>>,
        calls: Option<&CallsResult>,
        outcome: &Outcome,
        gas_used: u64,
    ) {
        let Some(message) = root else { return };
        let (reverted, output, gas_used) = match calls {
            Some(calls) => (calls.reverted, calls.output.clone(), gas_used),
            None => (true, Bytes::new(), outcome.gas_limit),
        };
        let mut result = MessageResult::<BaseEvmTypes> {
            stop: if reverted { InstrStop::Revert } else { InstrStop::Return },
            gas: GasTracker::new_used_gas(outcome.gas_limit.max(gas_used), gas_used, 0),
            output,
            ..Default::default()
        };
        Self::with_root_frame(evm, outcome, message, |inspector, frame| {
            inspector.call_end(frame, message, &mut result);
        });
    }

    /// Runs `hook` with the EVM's inspector and a top-level frame initialized with the root
    /// `message`, the parent frame evm2 hands the inspector for any top-level message. The
    /// inspector is detached for the hook's duration, so the frame can point at the EVM.
    fn with_root_frame(
        evm: &mut Evm<'_, BaseEvmTypes>,
        outcome: &Outcome,
        message: &Message<BaseEvmTypes>,
        hook: impl FnOnce(&mut dyn Inspector<BaseEvmTypes>, &mut Interpreter<'_, '_, BaseEvmTypes>),
    ) {
        let mut inspector = evm.clear_inspector().expect("the trace root needs an inspector");
        let tx_env = Self::tx_env(evm, outcome);
        let version = *evm.version();
        let mut frame = Interpreter::new(&tx_env, message);
        frame.prepare_run(evm.spec_id(), &version, evm);
        hook(inspector.as_mut(), &mut frame);
        drop(frame);
        evm.set_boxed_inspector(inspector);
    }

    /// The code of `address`.
    fn load_code(evm: &mut Evm<'_, BaseEvmTypes>, address: Address) -> HandlerResult<Bytecode> {
        evm.state_mut().account(&address, false).map_err(HandlerError::Fatal)?.load_code().map_err(HandlerError::Fatal)
    }

    /// Builds the top-level call frame for one protocol call: `caller` sends `value` to `to` with
    /// `data` and `gas_limit`. An EIP-7702 delegation at `to` runs the delegate's code, with
    /// precompiles disabled, as for the first frame of a normal transaction.
    fn call_message(
        evm: &mut Evm<'_, BaseEvmTypes>,
        caller: Address,
        to: Address,
        value: U256,
        data: Bytes,
        gas_limit: u64,
    ) -> HandlerResult<Message<BaseEvmTypes>> {
        let mut code = Self::load_code(evm, to)?;
        let mut code_address = to;
        let mut disable_precompiles = false;
        if let Some(delegate) = code.eip7702_address() {
            code = Self::load_code(evm, delegate)?;
            code_address = delegate;
            disable_precompiles = true;
        }
        Ok(Message::<BaseEvmTypes> {
            gas_limit,
            destination: to,
            caller,
            input: data,
            value,
            code,
            code_address,
            disable_precompiles,
            ..Default::default()
        })
    }

    /// Settles the fee against the pre-charge: refunds the payer's surplus and routes the base
    /// fee, priority fee, L1 data fee and operator fee. Returns the billed gas and the L1 fee.
    fn settle_fees(
        evm: &mut Evm<'_, BaseEvmTypes>,
        outcome: &Outcome,
        calls: &CallsResult,
        prepay: U256,
        enveloped: &Bytes,
    ) -> HandlerResult<(u64, U256)> {
        let spec = evm.config_spec_id();
        let upgrade = spec.upgrade();
        let billable_gas = if spec.is_feature_enabled(BaseFeature::GasLimitBilling) {
            outcome.max_chargeable_gas()
        } else {
            outcome.metered_gas_used(calls)
        };
        let overflow = || HandlerError::external(TxBaseError::FeeOverflow);
        let fee = U256::from(billable_gas)
            .checked_mul(U256::from(outcome.effective))
            .ok_or_else(overflow)?;
        let base_fee_amount = U256::from(billable_gas)
            .checked_mul(U256::from(outcome.base_fee))
            .ok_or_else(overflow)?;
        let priority_amount = fee.checked_sub(base_fee_amount).ok_or_else(overflow)?;
        let l1_cost = evm.ext().l1.calculate_tx_l1_cost(enveloped, upgrade);
        let operator_cost =
            evm.ext().l1.operator_fee_charge(enveloped, U256::from(billable_gas), upgrade);
        let total_cost = fee
            .checked_add(l1_cost)
            .and_then(|value| value.checked_add(operator_cost))
            .ok_or_else(overflow)?;
        let refund_amount = prepay.checked_sub(total_cost).ok_or_else(overflow)?;
        {
            let mut payer =
                evm.state_mut().account(&outcome.payer, false).map_err(HandlerError::Fatal)?;
            let balance = payer.balance().checked_add(refund_amount).ok_or_else(overflow)?;
            payer.set_balance(balance);
        }
        let beneficiary = evm.block().beneficiary;
        for (recipient, amount) in [
            (Predeploys::BASE_FEE_VAULT, base_fee_amount),
            (beneficiary, priority_amount),
            (Predeploys::L1_FEE_VAULT, l1_cost),
            (Predeploys::OPERATOR_FEE_VAULT, operator_cost),
        ] {
            let mut account =
                evm.state_mut().account(&recipient, false).map_err(HandlerError::Fatal)?;
            let _ = account.balance().checked_add(amount).ok_or_else(overflow)?;
            account.add_balance(amount);
        }
        Ok((billable_gas, l1_cost))
    }

    /// The pre-call context once `gas_limit` covers the EIP-7623 sender floor and the
    /// sender-intrinsic gas.
    fn outcome(
        signed: &TxBaseSigned,
        sender: Address,
        payer: Address,
        gas_limit: u64,
        base_fee: u128,
        bump_protocol_nonce: bool,
    ) -> HandlerResult<Outcome> {
        let tx = signed.tx();
        let intrinsic = IntrinsicGas::compute(signed, sender);
        if gas_limit < intrinsic.sender_floor() {
            return Err(HandlerError::external(TxBaseError::GasBelowFloor));
        }
        let execution_gas_available = intrinsic
            .execution_gas_available(gas_limit)
            .ok_or(HandlerError::external(TxBaseError::GasBelowIntrinsic))?;
        Ok(Outcome {
            sender,
            payer,
            gas_limit,
            sender_intrinsic: intrinsic.sender_intrinsic(),
            sender_floor: intrinsic.sender_floor(),
            payer_auth_cost: intrinsic.payer_auth_cost,
            execution_gas_available,
            effective: Self::effective_gas_price(tx.max_fee_per_gas, tx.max_priority_fee_per_gas, base_fee),
            base_fee,
            bump_protocol_nonce,
        })
    }

    /// EIP-1559 effective gas price: `min(max_fee, base_fee + tip)`.
    const fn effective_gas_price(max_fee: u128, tip: u128, base_fee: u128) -> u128 {
        let cap = base_fee.saturating_add(tip);
        if max_fee < cap { max_fee } else { cap }
    }
}

impl BaseEvmTypes {
    /// The registry handler for Base transactions (type `0x79`), registered from Everest.
    pub fn handle_base_tx(
        req: TxRequest<'_, '_, Self, TxBaseSigned>,
    ) -> HandlerResult<TxResult<Self>> {
        let envelope: &BaseTxEnvelope = req.envelope;
        assert_eq!(envelope.ty(), TxBaseConstants::BASE_TX_TYPE);
        let enveloped = envelope.enveloped().expect("a Base tx envelope carries its encoding");
        TxBaseExecutor::execute(req.host, *req.tx, enveloped)
    }
}

#[cfg(test)]
mod tests {
    //! Ported from the revm path's Base transaction tests (`crates/common/evm/src/base_tx.rs`);
    //! the expected receipts, gas, nonces and state are theirs.

    use alloy_consensus::transaction::Recovered;
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{bytes, keccak256};
    use base_common_base_tx::NonceManagerSlots;
    use base_common_consensus::{BaseTxEnvelope as SignedEnvelope, Call};
    use base_common_genesis::BaseUpgrade;
    use base_common_l1_fees::L1FeeParams;
    use evm2::{
        AccountInfo, BaseEvmConfigSelector, EvmConfigSelector, env::BlockEnv, evm::InMemoryDB,
    };
    use k256::ecdsa::SigningKey;

    use super::*;
    use crate::BaseSpecId;

    const CHAIN_ID: u64 = 8453;
    const NOW: u64 = 1_000;
    const BASE_FEE: u64 = 1_000_000_000;
    const BENEFICIARY: Address = address!("0x00000000000000000000000000000000000000bb");
    const WARMTH_PROBE_PAD_GAS: u64 = 5_000;
    const FLOOR_BURNER: Address = address!("0x00000000000000000000000000000000000000e5");

    fn eth() -> U256 {
        U256::from(10u64).pow(U256::from(18u64))
    }

    fn warmth_probe_loader_code() -> Bytes {
        let mut code = vec![0x5bu8; WARMTH_PROBE_PAD_GAS as usize];
        code.extend_from_slice(&[0x60, 0x00, 0x54, 0x00]);
        Bytes::from(code)
    }

    fn floor_burner_code() -> Bytes {
        let mut code = vec![0x5bu8; WARMTH_PROBE_PAD_GAS as usize];
        code.push(0x00);
        Bytes::from(code)
    }

    fn signing_key(byte: u8) -> SigningKey {
        SigningKey::from_slice(&[byte; 32]).unwrap()
    }

    fn eoa_address(key: &SigningKey) -> Address {
        let point = key.verifying_key().to_encoded_point(false);
        Address::from_slice(&keccak256(&point.as_bytes()[1..])[12..])
    }

    fn eoa_sig(key: &SigningKey, hash: B256) -> Bytes {
        let (signature, recid) = key.sign_prehash_recoverable(hash.as_slice()).unwrap();
        let mut out = vec![0u8; 65];
        out[..64].copy_from_slice(&signature.to_bytes());
        out[64] = recid.to_byte() + 27;
        Bytes::from(out)
    }

    fn base_tx() -> TxBase {
        TxBase {
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

    fn call(to: Address) -> Call {
        Call { to, value: U256::ZERO, data: Bytes::new() }
    }

    fn eoa_signed(tx: TxBase, key: &SigningKey) -> TxBaseSigned {
        let hash = tx.sender_signature_hash();
        TxBaseSigned::new(tx, eoa_sig(key, hash), Bytes::new())
    }

    fn named_signed(tx: TxBase, signer: &SigningKey) -> TxBaseSigned {
        let hash = tx.sender_signature_hash();
        let mut auth = TxBaseConstants::K1_AUTHENTICATOR.to_vec();
        auth.extend_from_slice(&eoa_sig(signer, hash));
        TxBaseSigned::new(tx, Bytes::from(auth), Bytes::new())
    }

    fn open_payer_signed(tx: TxBase, key: &SigningKey, payer_key: &SigningKey) -> TxBaseSigned {
        let sender = eoa_address(key);
        let sender_auth = eoa_sig(key, tx.sender_signature_hash());
        let payer_auth = eoa_sig(payer_key, tx.payer_signature_hash(sender));
        TxBaseSigned::new(tx, sender_auth, payer_auth)
    }

    /// Test fixture: one Everest EVM over an in-memory database.
    struct Fixture {
        upgrade: BaseUpgrade,
        db: InMemoryDB,
        fee_charge: bool,
        beneficiary: Address,
        timestamp: u64,
        l1: L1FeeParams,
    }

    impl Fixture {
        fn new(funded: Address, balance: U256, contracts: &[(Address, Bytes)]) -> Self {
            let mut fixture = Self {
                upgrade: BaseUpgrade::Everest,
                db: InMemoryDB::default(),
                fee_charge: true,
                beneficiary: BENEFICIARY,
                timestamp: NOW,
                l1: L1FeeParams::default(),
            };
            fixture.db.insert_account_info(&funded, AccountInfo { balance, ..Default::default() });
            for (address, code) in contracts {
                fixture.code(*address, code.clone());
            }
            fixture
        }

        fn code(&mut self, address: Address, code: Bytes) {
            let mut info = self.db.account_info(&address).cloned().unwrap_or_default();
            let code = Bytecode::new_raw(code);
            info.code_hash = code.hash_slow();
            info.code = Some(code);
            self.db.insert_account_info(&address, info);
        }

        fn storage(&mut self, address: Address, slot: U256, value: U256) {
            self.db.insert_account_storage(&address, &slot, &value);
        }

        fn nonce(&mut self, address: Address, nonce: u64) {
            let mut info = self.db.account_info(&address).cloned().unwrap_or_default();
            info.nonce = nonce;
            self.db.insert_account_info(&address, info);
        }

        fn evm(&self) -> Evm<'static, BaseEvmTypes> {
            let spec = BaseSpecId::new(self.upgrade);
            let config =
                <BaseEvmConfigSelector as EvmConfigSelector<BaseEvmTypes>>::execution_config(spec);
            let mut version = *config.version();
            version.chain_id = CHAIN_ID;
            version.features.set(EvmFeatures::FEE_CHARGE, self.fee_charge);
            let block = BlockEnv::<BaseEvmTypes> {
                number: U256::from(1u64),
                timestamp: U256::from(self.timestamp),
                basefee: U256::from(BASE_FEE),
                beneficiary: self.beneficiary,
                gas_limit: U256::from(60_000_000u64),
                ..Default::default()
            };
            let mut evm = Evm::new_with_execution_config(
                config.with_version(version),
                spec,
                block,
                BaseEvmTypes::tx_registry(spec),
                self.db.clone(),
                BaseEvmTypes::precompiles(spec, &crate::ChainSpec::base_mainnet()),
            );
            evm.ext_mut().l1 = self.l1;
            evm
        }
    }

    fn envelope(signed: &TxBaseSigned) -> Recovered<BaseTxEnvelope> {
        let encoded: Bytes = SignedEnvelope::Base(signed.clone()).encoded_2718().into();
        Recovered::new_unchecked(BaseTxEnvelope::base(signed.clone(), encoded), Address::ZERO)
    }

    fn transact(
        evm: &mut Evm<'static, BaseEvmTypes>,
        signed: &TxBaseSigned,
    ) -> HandlerResult<TxResult<BaseEvmTypes>> {
        evm.transact(&envelope(signed)).map(|executed| executed.commit())
    }

    fn simulate(
        evm: &mut Evm<'static, BaseEvmTypes>,
        signed: &TxBaseSigned,
        from: Address,
        mode: TxBaseSimulation,
    ) -> HandlerResult<TxResult<BaseEvmTypes>> {
        let result = TxBaseExecutor::simulate(evm, signed, from, mode);
        evm.state_mut().clear_transaction_state();
        result
    }

    fn metered_call(
        evm: &mut Evm<'static, BaseEvmTypes>,
        signed: &TxBaseSigned,
        from: Address,
    ) -> TxResult<BaseEvmTypes> {
        simulate(evm, signed, from, TxBaseSimulation::Call).expect("eth_call simulation runs")
    }

    fn info(evm: &mut Evm<'static, BaseEvmTypes>, address: Address) -> AccountInfo {
        evm.state_mut().account_info_untracked(&address).unwrap().unwrap_or_default()
    }

    fn balance(evm: &mut Evm<'static, BaseEvmTypes>, address: Address) -> U256 {
        info(evm, address).balance
    }

    fn slot(evm: &mut Evm<'static, BaseEvmTypes>, address: Address, key: U256) -> U256 {
        evm.state_mut().storage_slot_untracked(&address, &key).unwrap()
    }

    fn code_bytes(evm: &mut Evm<'static, BaseEvmTypes>, address: Address) -> Bytes {
        let info = info(evm, address);
        if info.code_hash == alloy_primitives::KECCAK256_EMPTY {
            return Bytes::new();
        }
        let code = match info.code {
            Some(code) if !code.is_empty() => code,
            _ => evm2::evm::DynDatabase::get_code_by_hash(
                evm.state_mut().overlay_db_mut(),
                &info.code_hash,
            )
            .unwrap(),
        };
        code.original_bytes()
    }

    fn reason(err: HandlerError) -> String {
        err.to_string()
    }

    #[test]
    fn eoa_self_pay_transaction_executes_and_charges_sender() {
        let key = signing_key(0x22);
        let sender = eoa_address(&key);
        let signed = eoa_signed(base_tx(), &key);
        let mut evm = Fixture::new(sender, eth(), &[]).evm();
        let result = transact(&mut evm, &signed).expect("Base tx executes");
        assert!(result.status);
        assert_eq!(result.tx_gas_used(), signed.tx().gas_limit, "gas-limit billing");
        assert_eq!(result.ext.committed_phases, 0);
        assert!(result.logs.is_empty());
        let sender_info = info(&mut evm, sender);
        assert_eq!(sender_info.nonce, 1);
        assert_eq!(sender_info.code_hash, alloy_primitives::KECCAK256_EMPTY);
        let gas = U256::from(signed.tx().gas_limit);
        let tip = U256::from(1_000_000_000u64);
        assert_eq!(sender_info.balance, eth() - gas * (U256::from(BASE_FEE) + tip));
        assert_eq!(balance(&mut evm, Predeploys::BASE_FEE_VAULT), gas * U256::from(BASE_FEE));
        assert_eq!(balance(&mut evm, BENEFICIARY), gas * tip);
    }

    #[test]
    fn eoa_call_transfers_value_to_recipient() {
        let key = signing_key(0x51);
        let sender = eoa_address(&key);
        let recipient = address!("0x00000000000000000000000000000000000000e1");
        let transfer = U256::from(1_000_000u64);
        let mut tx = base_tx();
        tx.calls = vec![vec![Call { to: recipient, value: transfer, data: Bytes::new() }]];
        let signed = eoa_signed(tx, &key);
        let mut evm = Fixture::new(sender, eth(), &[(recipient, bytes!("00"))]).evm();
        let result = transact(&mut evm, &signed).expect("value transfer runs");
        assert!(result.status);
        assert_eq!(result.ext.committed_phases, 1);
        assert_eq!(balance(&mut evm, recipient), transfer);
    }

    #[test]
    fn value_call_to_new_account_charges_account_creation() {
        let key = signing_key(0x53);
        let sender = eoa_address(&key);
        let recipient = address!("0x00000000000000000000000000000000000000e3");
        let mut tx = base_tx();
        tx.calls = vec![vec![
            Call { to: recipient, value: U256::from(1u64), data: Bytes::new() },
            call(FLOOR_BURNER),
        ]];
        let signed = eoa_signed(tx, &key);
        let metered = |accounts: &[(Address, Bytes)]| {
            let mut evm = Fixture::new(sender, eth(), accounts).evm();
            let result = metered_call(&mut evm, &signed, sender);
            assert!(result.status);
            result.tx_gas_used()
        };
        assert_eq!(
            metered(&[(FLOOR_BURNER, floor_burner_code())])
                - metered(&[(recipient, bytes!("00")), (FLOOR_BURNER, floor_burner_code())]),
            TxBaseGasSchedule::NEW_ACCOUNT_COST
        );
    }

    #[test]
    fn unaffordable_new_account_charge_does_not_burn_the_unspent_pool() {
        let key = signing_key(0x54);
        let sender = eoa_address(&key);
        let recipient = address!("0x00000000000000000000000000000000000000e4");
        let mut tx = base_tx();
        tx.calls = vec![vec![Call { to: recipient, value: U256::from(1u64), data: Bytes::new() }]];
        let intrinsic = IntrinsicGas::compute(&eoa_signed(tx.clone(), &key), sender);
        tx.gas_limit = intrinsic.sender_intrinsic() + TxBaseGasSchedule::NEW_ACCOUNT_COST - 1;
        let signed = eoa_signed(tx, &key);

        let fixture = Fixture::new(sender, eth(), &[]);
        let result = transact(&mut fixture.evm(), &signed).expect("tx is included");
        assert!(!result.status);
        assert_eq!(result.ext.committed_phases, 0);
        assert_eq!(result.tx_gas_used(), signed.tx().gas_limit);

        let metered = metered_call(&mut fixture.evm(), &signed, sender);
        assert!(!metered.status);
        assert_eq!(
            metered.tx_gas_used(),
            intrinsic.sender_intrinsic().max(intrinsic.sender_floor()),
            "unspent call gas must not be metered as spent"
        );
    }

    #[test]
    fn eoa_call_reverts_when_sender_cannot_cover_value() {
        let key = signing_key(0x52);
        let sender = eoa_address(&key);
        let recipient = address!("0x00000000000000000000000000000000000000e2");
        let mut tx = base_tx();
        tx.calls = vec![vec![Call { to: recipient, value: eth(), data: Bytes::new() }]];
        let signed = eoa_signed(tx, &key);
        let mut evm = Fixture::new(
            sender,
            U256::from(10u64).pow(U256::from(16u64)),
            &[(recipient, bytes!("00"))],
        )
        .evm();
        let result = transact(&mut evm, &signed).expect("tx is included");
        assert!(!result.status);
        assert!(balance(&mut evm, recipient).is_zero());
        assert_eq!(info(&mut evm, sender).nonce, 1);
    }

    #[test]
    fn foreign_chain_id_is_rejected_and_matching_executes() {
        let key = signing_key(0xa1);
        let sender = eoa_address(&key);
        let fixture = Fixture::new(sender, eth(), &[]);
        let foreign = eoa_signed(TxBase { chain_id: 1, ..base_tx() }, &key);
        let err = transact(&mut fixture.evm(), &foreign).unwrap_err();
        assert!(reason(err).contains("chain id mismatch"));
        let mut evm = fixture.evm();
        assert!(transact(&mut evm, &eoa_signed(base_tx(), &key)).unwrap().status);
        assert_eq!(info(&mut evm, sender).nonce, 1);
    }

    #[test]
    fn saturated_protocol_nonce_is_rejected() {
        let key = signing_key(0x21);
        let sender = eoa_address(&key);
        let signed = eoa_signed(TxBase { nonce_sequence: u64::MAX, ..base_tx() }, &key);
        let mut fixture = Fixture::new(sender, eth(), &[]);
        fixture.nonce(sender, u64::MAX);
        let err = transact(&mut fixture.evm(), &signed).unwrap_err();
        assert!(reason(err).contains("u64::MAX"));
    }

    #[test]
    fn two_dimensional_nonce_is_incremented() {
        let key = signing_key(0x2a);
        let sender = eoa_address(&key);
        let nonce_key = U256::from(7);
        let nonce_slot = NonceManagerSlots::nonce_slot(sender, nonce_key).unwrap();
        let mut tx = base_tx();
        tx.nonce_key = nonce_key;
        tx.nonce_sequence = 3;
        let signed = eoa_signed(tx, &key);
        let mut fixture = Fixture::new(sender, eth(), &[]);
        fixture.storage(NonceManagerSlots::ADDRESS, nonce_slot, U256::from(3));
        let mut evm = fixture.evm();
        let result = transact(&mut evm, &signed).expect("Base tx executes");
        assert!(result.status);
        assert!(result.logs.is_empty(), "the nonce increment adds no log");
        assert_eq!(slot(&mut evm, NonceManagerSlots::ADDRESS, nonce_slot), U256::from(4));
        assert_eq!(info(&mut evm, sender).nonce, 0, "a 2D nonce leaves the account nonce");
        // The nonce manager account carries the reference's `0xEF` stub.
        assert_eq!(code_bytes(&mut evm, NonceManagerSlots::ADDRESS), bytes!("ef"));

        // The consumed sequence cannot be replayed; the next one is ready.
        let err = transact(&mut evm, &signed).unwrap_err();
        assert!(reason(err).contains("below the channel nonce"));
        let next = eoa_signed(TxBase { nonce_key, nonce_sequence: 4, ..base_tx() }, &key);
        assert!(transact(&mut evm, &next).unwrap().status);
    }

    /// Execution publishes the sender to the `TxContext` precompile:
    /// `getTransactionSenderActorId()` returns the sender address right-aligned in a word.
    #[test]
    fn calls_read_the_sender_actor_id_from_tx_context() {
        let key = signing_key(0x53);
        let sender = eoa_address(&key);
        let reader = address!("0x00000000000000000000000000000000000000e2");

        // Calls `getTransactionSenderActorId()` and stores the returned word in slot 0.
        let mut code = vec![0x63];
        code.extend_from_slice(&keccak256("getTransactionSenderActorId()")[..4]);
        // PUSH1 0xe0 SHL PUSH1 0 MSTORE: the selector at memory[0..4].
        code.extend_from_slice(&[0x60, 0xe0, 0x1b, 0x60, 0x00, 0x52]);
        // CALL(gas, TxContext, 0, 0, 4, 0, 32).
        code.extend_from_slice(&[0x60, 0x20, 0x60, 0x00, 0x60, 0x04, 0x60, 0x00, 0x60, 0x00, 0x73]);
        code.extend_from_slice(TX_CONTEXT_ADDRESS.as_slice());
        code.extend_from_slice(&[0x5a, 0xf1]);
        // POP PUSH1 0 MLOAD PUSH1 0 SSTORE STOP.
        code.extend_from_slice(&[0x50, 0x60, 0x00, 0x51, 0x60, 0x00, 0x55, 0x00]);

        let mut tx = base_tx();
        tx.calls = vec![vec![call(reader)]];
        let signed = eoa_signed(tx, &key);
        let mut evm = Fixture::new(sender, eth(), &[(reader, Bytes::from(code))]).evm();
        let result = transact(&mut evm, &signed).expect("Base tx executes");

        assert!(result.status);
        assert_eq!(B256::from(slot(&mut evm, reader, U256::ZERO)), sender.into_word());
    }

    /// A call that reads its own 2D nonce during simulation sees the post-increment sequence,
    /// as it would on inclusion.
    #[test]
    fn simulate_calls_see_the_advanced_two_dimensional_nonce() {
        let key = signing_key(0x2b);
        let sender = eoa_address(&key);
        let reader = address!("0x00000000000000000000000000000000000000ab");
        let nonce_key = 7u8;
        let current_nonce = 3u8;

        // STATICCALL getNonce(sender, nonce_key) and revert unless it returns current_nonce + 1.
        let mut code = vec![0x63];
        code.extend_from_slice(&keccak256("getNonce(address,uint256)")[..4]);
        code.extend_from_slice(&[0x60, 0xe0, 0x1b, 0x60, 0x00, 0x52, 0x73]);
        code.extend_from_slice(sender.as_slice());
        code.extend_from_slice(&[0x60, 0x04, 0x52, 0x60, nonce_key, 0x60, 0x24, 0x52]);
        code.extend_from_slice(&[0x60, 0x20, 0x60, 0x00, 0x60, 0x44, 0x60, 0x00, 0x73]);
        code.extend_from_slice(NonceManagerSlots::ADDRESS.as_slice());
        code.extend_from_slice(&[0x5a, 0xfa, 0x50, 0x60, 0x00, 0x51, 0x60, current_nonce + 1]);
        let ok = u8::try_from(code.len() + 8).unwrap();
        code.extend_from_slice(&[0x14, 0x60, ok, 0x57, 0x60, 0x00, 0x80, 0xfd, 0x5b, 0x00]);

        let mut tx = base_tx();
        tx.nonce_key = U256::from(nonce_key);
        tx.nonce_sequence = current_nonce.into();
        tx.calls = vec![vec![call(reader)]];
        let signed = eoa_signed(tx, &key);
        let nonce_slot = NonceManagerSlots::nonce_slot(sender, U256::from(nonce_key)).unwrap();
        let mut fixture = Fixture::new(sender, eth(), &[(reader, Bytes::from(code))]);
        fixture.storage(NonceManagerSlots::ADDRESS, nonce_slot, U256::from(current_nonce));
        let mut evm = fixture.evm();

        let result = metered_call(&mut evm, &signed, sender);
        assert!(result.status, "the call must observe nonce {}", current_nonce + 1);
        // Inclusion sees the same advanced nonce.
        let result = transact(&mut evm, &signed).expect("Base tx executes");
        assert!(result.status, "inclusion must observe nonce {}", current_nonce + 1);
    }

    /// Records the call tree an inspector sees: `(depth, caller, destination, input, gas limit)`
    /// on `call`, and `(stop, gas spent)` on `call_end`, in hook order.
    #[derive(Debug, Default)]
    struct CallTree {
        events: Vec<CallEvent>,
        open: usize,
    }

    #[derive(Debug, PartialEq, Eq)]
    enum CallEvent {
        Call { depth: usize, caller: Address, to: Address, input: Bytes, gas_limit: u64 },
        End { depth: usize, stop: InstrStop, gas_spent: u64 },
    }

    impl Inspector<BaseEvmTypes> for CallTree {
        fn call(
            &mut self,
            _: &mut Interpreter<'_, '_, BaseEvmTypes>,
            message: &mut Message<BaseEvmTypes>,
        ) -> Option<MessageResult<BaseEvmTypes>> {
            self.events.push(CallEvent::Call {
                depth: self.open,
                caller: message.caller,
                to: message.destination,
                input: message.input.clone(),
                gas_limit: message.gas_limit,
            });
            self.open += 1;
            None
        }

        fn call_end(
            &mut self,
            _: &mut Interpreter<'_, '_, BaseEvmTypes>,
            _: &Message<BaseEvmTypes>,
            result: &mut MessageResult<BaseEvmTypes>,
        ) {
            self.open -= 1;
            self.events.push(CallEvent::End {
                depth: self.open,
                stop: result.stop,
                gas_spent: result.gas.limit() - result.gas.remaining(),
            });
        }
    }

    fn traced(evm: &mut Evm<'static, BaseEvmTypes>) -> Vec<CallEvent> {
        let tree = evm.clear_inspector_as::<CallTree>().expect("the CallTree inspector");
        assert_eq!(tree.open, 0, "every opened frame closes");
        tree.events
    }

    /// With an inspector installed, a Base transaction is one synthetic root frame (sender to
    /// itself, the enveloped transaction as input, the sender's gas limit) whose children are
    /// the root calls; the root closes with the receipt's status and gas.
    #[test]
    fn inspected_base_tx_is_one_trace_root_over_its_calls() {
        let key = signing_key(0x61);
        let sender = eoa_address(&key);
        let first = address!("0x00000000000000000000000000000000000000f1");
        let second = address!("0x00000000000000000000000000000000000000f2");
        let reverter = address!("0x00000000000000000000000000000000000000f3");
        let mut tx = base_tx();
        tx.calls = vec![vec![call(first), call(second)], vec![call(reverter)]];
        let gas_limit = tx.gas_limit;
        let signed = eoa_signed(tx, &key);
        let contracts =
            [(first, bytes!("00")), (second, bytes!("00")), (reverter, bytes!("60006000fd"))];
        let fixture = Fixture::new(sender, eth(), &contracts);

        // The same transaction without an inspector: inspection must not change execution.
        let plain = transact(&mut fixture.evm(), &signed).unwrap();
        let mut evm = fixture.evm();
        evm.set_inspector(CallTree::default());
        let result = transact(&mut evm, &signed).unwrap();
        assert!(!result.status);
        assert_eq!(result.tx_gas_used(), plain.tx_gas_used());
        assert_eq!(result.ext.committed_phases, 1);

        let events = traced(&mut evm);
        assert_eq!(events.len(), 8, "{events:#?}");
        assert_eq!(
            events[0],
            CallEvent::Call {
                depth: 0,
                caller: sender,
                to: sender,
                input: envelope(&signed).inner().enveloped().unwrap().clone(),
                gas_limit,
            }
        );
        for (event, target) in [(&events[1], first), (&events[3], second), (&events[5], reverter)] {
            let CallEvent::Call { depth, caller, to, .. } = event else {
                panic!("expected a root call, got {event:?}");
            };
            assert_eq!((*depth, *caller, *to), (1, sender, target));
        }
        assert!(matches!(events[6], CallEvent::End { depth: 1, stop: InstrStop::Revert, .. }));
        assert_eq!(
            events[7],
            CallEvent::End { depth: 0, stop: InstrStop::Revert, gas_spent: result.tx_gas_used() }
        );
    }

    /// Under gas-limit billing a sponsored transaction settles `gas_limit + payer_auth_cost`,
    /// more than the sender's `gas_limit`. The trace root reports that billed gas, as the
    /// receipt does, instead of capping it at `gas_limit`.
    #[test]
    fn gas_limit_billing_trace_root_reports_the_billed_gas_of_a_sponsored_tx() {
        let key = signing_key(0x3c);
        let sender = eoa_address(&key);
        let payer_key = signing_key(0x3d);
        let payer = eoa_address(&payer_key);
        let target = address!("0x00000000000000000000000000000000000000cc");
        let tx = TxBase {
            payer: Some(TxBaseConstants::OPEN_PAYER),
            calls: vec![vec![call(target)]],
            ..base_tx()
        };
        let gas_limit = tx.gas_limit;
        let signed = open_payer_signed(tx, &key, &payer_key);
        let payer_auth_cost = IntrinsicGas::compute(&signed, sender).payer_auth_cost;
        assert!(payer_auth_cost > 0, "a sponsored transaction reserves payer authentication");

        let mut evm = settlement_fixture(payer, &[(target, bytes!("00"))]).evm();
        evm.set_inspector(CallTree::default());
        let result = transact(&mut evm, &signed).unwrap();
        assert!(result.status);
        assert_eq!(result.tx_gas_used(), gas_limit + payer_auth_cost);
        let events = traced(&mut evm);
        assert_eq!(
            events.last(),
            Some(&CallEvent::End {
                depth: 0,
                stop: InstrStop::Return,
                gas_spent: gas_limit + payer_auth_cost
            }),
            "the trace root must report the billed gas, not the sender's gas limit"
        );
    }

    /// An inspected estimate traces only its final run, not the search's probes, and the root
    /// reports the estimate.
    #[test]
    fn inspected_estimate_traces_only_the_final_run() {
        let key = signing_key(0x62);
        let sender = eoa_address(&key);
        let mut tx = base_tx();
        tx.calls = vec![vec![call(FLOOR_BURNER)]];
        let signed = eoa_signed(tx, &key);
        let fixture = Fixture::new(sender, eth(), &[(FLOOR_BURNER, floor_burner_code())]);

        let plain = simulate(&mut fixture.evm(), &signed, sender, TxBaseSimulation::Estimate)
            .unwrap()
            .tx_gas_used();
        let mut evm = fixture.evm();
        evm.set_inspector(CallTree::default());
        let estimate = simulate(&mut evm, &signed, sender, TxBaseSimulation::Estimate).unwrap();
        assert_eq!(estimate.tx_gas_used(), plain);

        let events = traced(&mut evm);
        assert_eq!(events.len(), 4, "the root and one burner run: {events:#?}");
        assert_eq!(
            events[3],
            CallEvent::End { depth: 0, stop: InstrStop::Return, gas_spent: plain }
        );
    }

    #[test]
    fn delegation_over_ordinary_sender_code_is_rejected_without_replacing_it() {
        let key = signing_key(0x23);
        let sender = eoa_address(&key);
        let ordinary = bytes!("60006000");
        let mut tx = base_tx();
        tx.sender = Some(sender);
        tx.account_changes = vec![AccountChange::Delegation(Delegation {
            target: address!("0x00000000000000000000000000000000000000dd"),
        })];
        let signed = named_signed(tx, &key);
        let mut fixture = Fixture::new(sender, eth(), &[]);
        fixture.code(sender, ordinary.clone());
        let mut evm = fixture.evm();
        let err = transact(&mut evm, &signed).unwrap_err();
        assert!(reason(err).contains("delegation cannot replace non-delegation code"));
        assert_eq!(code_bytes(&mut evm, sender), ordinary);

        let err = simulate(&mut evm, &signed, sender, TxBaseSimulation::Estimate).unwrap_err();
        assert!(reason(err).contains("delegation cannot replace non-delegation code"));
        assert_eq!(code_bytes(&mut evm, sender), ordinary);
    }

    #[test]
    fn stale_nonce_is_rejected_before_account_changes() {
        let key = signing_key(0x25);
        let sender = eoa_address(&key);
        let mut tx = base_tx();
        tx.sender = Some(sender);
        tx.account_changes = vec![AccountChange::Delegation(Delegation {
            target: address!("0x00000000000000000000000000000000000000dd"),
        })];
        let signed = named_signed(tx, &key);
        let mut fixture = Fixture::new(sender, eth(), &[]);
        fixture.code(sender, bytes!("60006000"));
        fixture.nonce(sender, 1);
        let err = transact(&mut fixture.evm(), &signed).unwrap_err();
        assert!(reason(err).contains("below the channel nonce"));
    }

    #[test]
    fn delegation_requires_protocol_nonce() {
        let key = signing_key(0x27);
        let sender = eoa_address(&key);
        let target = address!("0x00000000000000000000000000000000000000dd");
        let signed = |nonce_key: U256| {
            let mut tx = base_tx();
            tx.nonce_key = nonce_key;
            tx.account_changes = vec![AccountChange::Delegation(Delegation { target })];
            eoa_signed(tx, &key)
        };
        let fixture = Fixture::new(sender, eth(), &[]);
        let mut evm = fixture.evm();
        assert!(transact(&mut evm, &signed(U256::ZERO)).unwrap().status);
        assert_eq!(info(&mut evm, sender).nonce, 1);
        assert_eq!(code_bytes(&mut evm, sender), Bytecode::new_eip7702(target).original_bytes());
        for nonce_key in [U256::from(7), TxBaseConstants::NONCE_KEY_MAX] {
            let err = transact(&mut fixture.evm(), &signed(nonce_key)).unwrap_err();
            assert!(reason(err).contains("must use the protocol nonce"));
        }
    }

    #[test]
    fn explicit_zero_delegation_clears_without_logs() {
        let key = signing_key(0x24);
        let sender = eoa_address(&key);
        let mut tx = base_tx();
        tx.account_changes = vec![AccountChange::Delegation(Delegation { target: Address::ZERO })];
        let signed = eoa_signed(tx, &key);
        let mut fixture = Fixture::new(sender, eth(), &[]);
        fixture.code(
            sender,
            Bytecode::new_eip7702(address!("0x00000000000000000000000000000000000000dd"))
                .original_bytes(),
        );
        let sim = simulate(&mut fixture.evm(), &signed, sender, TxBaseSimulation::Estimate)
            .expect("clear simulates");
        assert!(sim.status && sim.logs.is_empty());
        let mut evm = fixture.evm();
        let result = transact(&mut evm, &signed).expect("clear executes");
        assert!(result.status && result.logs.is_empty());
        assert_eq!(info(&mut evm, sender).code_hash, alloy_primitives::KECCAK256_EMPTY);
    }

    #[test]
    fn simulate_estimate_covers_execution_gas_without_a_signature() {
        let key = signing_key(0x77);
        let sender = eoa_address(&key);
        let target = address!("0x00000000000000000000000000000000000000c5");
        let mut tx = base_tx();
        tx.calls = vec![vec![call(target)]];
        let signed = eoa_signed(tx, &key);
        let fixture = Fixture::new(sender, eth(), &[(target, bytes!("00"))]);
        let settled = transact(&mut fixture.evm(), &signed).unwrap().tx_gas_used();
        let metered = metered_call(&mut fixture.evm(), &signed, sender).tx_gas_used();
        let mut evm = fixture.evm();
        let estimate = simulate(&mut evm, &signed, sender, TxBaseSimulation::Estimate).unwrap();
        assert!(estimate.status);
        assert_eq!(estimate.tx_gas_used(), metered);
        assert_eq!(settled, base_tx().gas_limit);
        // Simulation commits nothing: the nonce is still unused.
        assert_eq!(info(&mut evm, sender).nonce, 0);
        assert!(transact(&mut evm, &signed).unwrap().status);
    }

    /// Operator fee scalar for the settlement tests (post-Jovian `gas * scalar * 100`).
    const OPERATOR_FEE_SCALAR: u64 = 1;

    fn operator_fee(gas: u64) -> U256 {
        U256::from(gas) * U256::from(OPERATOR_FEE_SCALAR) * U256::from(100)
    }

    fn settlement_fixture(payer: Address, contracts: &[(Address, Bytes)]) -> Fixture {
        let mut fixture = Fixture::new(payer, eth(), contracts);
        fixture.l1.operator_fee_scalar = Some(U256::from(OPERATOR_FEE_SCALAR));
        fixture.l1.operator_fee_constant = Some(U256::ZERO);
        fixture
    }

    struct Settlement {
        success: bool,
        gas_used: u64,
        paid: U256,
        tip: U256,
        burned: U256,
        l1_fee: U256,
        operator_fee: U256,
        sender_nonce: u64,
        payer_nonce: u64,
    }

    impl Settlement {
        fn received(&self) -> U256 {
            self.tip + self.burned + self.l1_fee + self.operator_fee
        }
    }

    fn settle(
        fixture: &Fixture,
        signed: &TxBaseSigned,
        sender: Address,
        payer: Address,
    ) -> Settlement {
        let mut evm = fixture.evm();
        let result = transact(&mut evm, signed).expect("tx executes");
        Settlement {
            success: result.status,
            gas_used: result.tx_gas_used(),
            paid: eth() - balance(&mut evm, payer),
            tip: balance(&mut evm, BENEFICIARY),
            burned: balance(&mut evm, Predeploys::BASE_FEE_VAULT),
            l1_fee: balance(&mut evm, Predeploys::L1_FEE_VAULT),
            operator_fee: balance(&mut evm, Predeploys::OPERATOR_FEE_VAULT),
            sender_nonce: info(&mut evm, sender).nonce,
            payer_nonce: info(&mut evm, payer).nonce,
        }
    }

    #[test]
    fn gas_limit_billing_settles_self_pay_at_the_declared_limit() {
        let key = signing_key(0x78);
        let sender = eoa_address(&key);
        let target = address!("0x00000000000000000000000000000000000000c6");
        for code in [bytes!("00"), bytes!("60006000fd")] {
            let tx = TxBase { calls: vec![vec![call(target)]], ..base_tx() };
            let gas_limit = tx.gas_limit;
            let priority = tx.max_priority_fee_per_gas;
            let effective = u128::from(BASE_FEE) + priority;
            let signed = eoa_signed(tx, &key);
            let fixture = settlement_fixture(sender, &[(target, code.clone())]);
            let metered = metered_call(&mut fixture.evm(), &signed, sender);
            assert!(metered.tx_gas_used() < gas_limit);
            let billed = settle(&fixture, &signed, sender, sender);
            assert_eq!(billed.success, metered.status);
            assert_eq!(billed.gas_used, gas_limit);
            assert_eq!(billed.tip, U256::from(priority) * U256::from(gas_limit));
            assert_eq!(billed.burned, U256::from(BASE_FEE) * U256::from(gas_limit));
            assert_eq!(billed.operator_fee, operator_fee(gas_limit));
            assert_eq!(
                billed.paid,
                U256::from(effective) * U256::from(gas_limit) + billed.l1_fee + billed.operator_fee
            );
            assert_eq!(billed.paid, billed.received(), "billing neither mints nor burns");
        }
    }

    #[test]
    fn gas_limit_billing_estimate_excludes_payer_auth_so_it_is_billed_once() {
        let key = signing_key(0x38);
        let sender = eoa_address(&key);
        let payer_key = signing_key(0x39);
        let payer = eoa_address(&payer_key);
        let target = address!("0x00000000000000000000000000000000000000ca");
        let contracts = [(target, bytes!("00"))];
        let tx = TxBase {
            payer: Some(TxBaseConstants::OPEN_PAYER),
            calls: vec![vec![call(target)]],
            ..base_tx()
        };
        let effective = u128::from(BASE_FEE) + tx.max_priority_fee_per_gas;
        let placeholder = Bytes::from([0xff_u8; 65]);
        let unsigned = TxBaseSigned::new(tx.clone(), placeholder.clone(), placeholder);
        let estimated = IntrinsicGas::compute(&unsigned, sender);
        assert!(estimated.payer_auth_cost > 0);

        let fixture = settlement_fixture(payer, &contracts);
        let estimate = simulate(&mut fixture.evm(), &unsigned, sender, TxBaseSimulation::Estimate)
            .unwrap()
            .tx_gas_used();
        assert_eq!(estimate, estimated.sender_intrinsic().max(estimated.sender_floor()));

        let declared = open_payer_signed(TxBase { gas_limit: estimate, ..tx }, &key, &payer_key);
        let actual = IntrinsicGas::compute(&declared, sender);
        let billed = settle(&fixture, &declared, sender, payer);
        assert!(billed.success, "`gas_limit = estimate` suffices");
        assert_eq!(billed.gas_used, estimate + actual.payer_auth_cost);
        assert_eq!(
            billed.paid,
            U256::from(effective) * U256::from(billed.gas_used)
                + billed.l1_fee
                + operator_fee(billed.gas_used)
        );
        assert_eq!(billed.paid, billed.received());
    }

    #[test]
    fn fee_charge_disabled_execution_meters_gas_and_moves_no_fees() {
        let key = signing_key(0x7a);
        let sender = eoa_address(&key);
        let target = address!("0x00000000000000000000000000000000000000c8");
        let value = U256::from(7);
        let tx = TxBase {
            calls: vec![vec![Call { to: target, value, data: Bytes::new() }]],
            ..base_tx()
        };
        let gas_limit = tx.gas_limit;
        let signed = eoa_signed(tx, &key);
        let mut fixture = settlement_fixture(sender, &[(target, bytes!("00"))]);
        let metered = metered_call(&mut fixture.evm(), &signed, sender).tx_gas_used();
        assert!(metered < gas_limit);
        let billed = settle(&fixture, &signed, sender, sender);
        assert_eq!(billed.gas_used, gas_limit);
        assert_eq!(billed.paid - value, billed.received());

        fixture.fee_charge = false;
        let unbilled = settle(&fixture, &signed, sender, sender);
        assert!(unbilled.success);
        assert_eq!(unbilled.gas_used, metered);
        assert_eq!(unbilled.paid, value);
        assert_eq!(unbilled.received(), U256::ZERO);
        assert_eq!(unbilled.sender_nonce, 1);
    }

    #[test]
    fn fee_charge_disabled_execution_does_not_debit_a_sponsoring_payer() {
        let key = signing_key(0x7b);
        let sender = eoa_address(&key);
        let payer_key = signing_key(0x7c);
        let payer = eoa_address(&payer_key);
        let target = address!("0x00000000000000000000000000000000000000c9");
        let tx = TxBase {
            payer: Some(TxBaseConstants::OPEN_PAYER),
            calls: vec![vec![call(target)]],
            ..base_tx()
        };
        let gas_limit = tx.gas_limit;
        let signed = open_payer_signed(tx, &key, &payer_key);
        let intrinsic = IntrinsicGas::compute(&signed, sender);
        let mut fixture = settlement_fixture(payer, &[(target, bytes!("00"))]);
        let metered = metered_call(&mut fixture.evm(), &signed, sender).tx_gas_used();
        let billed = settle(&fixture, &signed, sender, payer);
        assert!(billed.success);
        assert_eq!(billed.gas_used, gas_limit + intrinsic.payer_auth_cost);
        assert_eq!(billed.paid, billed.received());
        assert!(billed.paid > U256::ZERO);
        assert_eq!(billed.sender_nonce, 1);

        fixture.fee_charge = false;
        let unbilled = settle(&fixture, &signed, sender, payer);
        assert_eq!(unbilled.gas_used, metered);
        assert_eq!(unbilled.paid, U256::ZERO);
        assert_eq!(unbilled.received(), U256::ZERO);
        assert_eq!(unbilled.payer_nonce, 0);
        assert_eq!(unbilled.sender_nonce, 1);
    }

    #[test]
    fn fee_charge_disabled_execution_nets_the_capped_refund() {
        let key = signing_key(0x7d);
        let sender = eoa_address(&key);
        let target = address!("0x00000000000000000000000000000000000000cb");
        let mut code = vec![0x5bu8; WARMTH_PROBE_PAD_GAS as usize];
        code.extend_from_slice(&[0x60, 0x01, 0x60, 0x00, 0x55, 0x60, 0x00, 0x60, 0x00, 0x55, 0x00]);
        const CALL_GAS_SPENT: u64 = WARMTH_PROBE_PAD_GAS + 4 * 3 + 2_100 + 20_000 + 100;
        const RESTORE_ZERO_REFUND: u64 = 20_000 - 100;
        let tx = TxBase { calls: vec![vec![call(target)]], ..base_tx() };
        let signed = eoa_signed(tx, &key);
        let intrinsic = IntrinsicGas::compute(&signed, sender);
        let gross = intrinsic.sender_intrinsic() + CALL_GAS_SPENT;
        let capped_refund = RESTORE_ZERO_REFUND.min(gross / 5);
        let net = gross - capped_refund;
        assert!(net > intrinsic.sender_floor());

        let mut fixture = settlement_fixture(sender, &[(target, Bytes::from(code))]);
        let metered = metered_call(&mut fixture.evm(), &signed, sender);
        assert!(metered.status);
        assert_eq!(metered.tx_gas_used(), gross, "`Call` simulation is gross of refund");
        fixture.fee_charge = false;
        let mut evm = fixture.evm();
        let result = transact(&mut evm, &signed).unwrap();
        assert_eq!(result.tx_gas_used(), net);
        assert_eq!(slot(&mut evm, target, U256::ZERO), U256::ZERO);
    }

    #[test]
    fn fee_charge_disabled_execution_skips_the_payer_balance_check() {
        let key = signing_key(0x7e);
        let sender = eoa_address(&key);
        let target = address!("0x00000000000000000000000000000000000000cc");
        let value = U256::from(5);
        let tx = TxBase {
            calls: vec![vec![Call { to: target, value, data: Bytes::new() }]],
            ..base_tx()
        };
        let signed = eoa_signed(tx, &key);
        let mut fixture = Fixture::new(sender, value, &[(target, bytes!("00"))]);
        assert_eq!(
            transact(&mut fixture.evm(), &signed).unwrap_err(),
            HandlerError::InsufficientFunds
        );
        fixture.fee_charge = false;
        let metered = metered_call(&mut fixture.evm(), &signed, sender).tx_gas_used();
        let mut evm = fixture.evm();
        let result = transact(&mut evm, &signed).expect("fee-free execution");
        assert!(result.status);
        assert_eq!(result.tx_gas_used(), metered);
        assert_eq!(balance(&mut evm, sender), U256::ZERO);
        assert_eq!(balance(&mut evm, target), value);
        assert_eq!(balance(&mut evm, BENEFICIARY), U256::ZERO);
        assert_eq!(info(&mut evm, sender).nonce, 1);
    }

    #[test]
    fn simulate_estimate_covers_gross_gas_when_sstore_refund_earned() {
        let key = signing_key(0x7b);
        let sender = eoa_address(&key);
        let target = address!("0x00000000000000000000000000000000000000e1");
        let mut fixture = Fixture::new(sender, eth(), &[(target, bytes!("600060005500"))]);
        fixture.storage(target, U256::ZERO, U256::from(1u64));
        let with_limit = |gas_limit: u64| {
            eoa_signed(TxBase { gas_limit, calls: vec![vec![call(target)]], ..base_tx() }, &key)
        };
        let reference = with_limit(base_tx().gas_limit);
        let gross = metered_call(&mut fixture.evm(), &reference, sender).tx_gas_used();
        let floor = IntrinsicGas::compute(&reference, sender).sender_floor();
        let charge = (gross - 4_800).max(floor);
        let estimate = simulate(&mut fixture.evm(), &reference, sender, TxBaseSimulation::Estimate)
            .unwrap()
            .tx_gas_used();
        assert!(estimate >= gross && estimate > charge);
        assert!(!transact(&mut fixture.evm(), &with_limit(charge)).unwrap().status);
        assert!(transact(&mut fixture.evm(), &with_limit(estimate)).unwrap().status);
    }

    #[test]
    fn simulate_estimate_covers_63_64_gas_retention_in_nested_calls() {
        let key = signing_key(0x7c);
        let sender = eoa_address(&key);
        let forwarder = address!("0x00000000000000000000000000000000000000e2");
        let sink = address!("0x00000000000000000000000000000000000000e3");
        let sink_code = bytes!("60645b80545060019003806002575000");
        let mut fwd = bytes!("6000600060006000600073").to_vec();
        fwd.extend_from_slice(sink.as_slice());
        fwd.extend_from_slice(bytes!("5af115602657005b60006000fd").as_ref());
        let fixture =
            Fixture::new(sender, eth(), &[(forwarder, Bytes::from(fwd)), (sink, sink_code)]);
        let with_limit = |gas_limit: u64| {
            eoa_signed(TxBase { gas_limit, calls: vec![vec![call(forwarder)]], ..base_tx() }, &key)
        };
        let reference = with_limit(base_tx().gas_limit);
        let metered = metered_call(&mut fixture.evm(), &reference, sender);
        assert!(metered.status);
        let charge = metered.tx_gas_used();
        let estimate = simulate(&mut fixture.evm(), &reference, sender, TxBaseSimulation::Estimate)
            .unwrap()
            .tx_gas_used();
        assert!(estimate > charge);
        assert!(!transact(&mut fixture.evm(), &with_limit(charge)).unwrap().status);
        assert!(transact(&mut fixture.evm(), &with_limit(estimate)).unwrap().status);
    }

    #[test]
    fn open_payer_is_charged_instead_of_the_sender() {
        let key = signing_key(0x34);
        let sender = eoa_address(&key);
        let payer_key = signing_key(0x35);
        let payer = eoa_address(&payer_key);
        let tx = TxBase { payer: Some(TxBaseConstants::OPEN_PAYER), ..base_tx() };
        let signed = open_payer_signed(tx, &key, &payer_key);
        let mut evm = Fixture::new(payer, eth(), &[]).evm();
        assert!(transact(&mut evm, &signed).unwrap().status);
        assert!(balance(&mut evm, payer) < eth());
        assert!(balance(&mut evm, sender).is_zero());
        assert_eq!(info(&mut evm, sender).nonce, 1);
    }

    #[test]
    fn sponsored_k1_payer_settles_at_the_k1_authentication_cost() {
        let key = signing_key(0x36);
        let sender = eoa_address(&key);
        let payer_key = signing_key(0x37);
        let payer = eoa_address(&payer_key);
        let loader = address!("0x00000000000000000000000000000000000000c9");
        let tx = TxBase {
            payer: Some(TxBaseConstants::OPEN_PAYER),
            calls: vec![vec![call(loader)]],
            ..base_tx()
        };
        let signed = open_payer_signed(tx, &key, &payer_key);
        let intrinsic = IntrinsicGas::compute(&signed, sender);
        assert_eq!(intrinsic.payer_auth_cost, TxBaseGasSchedule::K1_AUTH_COST);
        let fixture = Fixture::new(payer, eth(), &[(loader, warmth_probe_loader_code())]);
        let result = transact(&mut fixture.evm(), &signed).unwrap();
        assert!(result.status);
        assert_eq!(result.tx_gas_used(), signed.tx().gas_limit + intrinsic.payer_auth_cost);
        let sender_gas =
            intrinsic.sender_intrinsic() + WARMTH_PROBE_PAD_GAS + 3 + TxBaseGasSchedule::COLD_SLOAD;
        assert!(sender_gas > intrinsic.sender_floor());
        let metered = metered_call(&mut fixture.evm(), &signed, sender);
        assert_eq!(metered.tx_gas_used(), sender_gas + intrinsic.payer_auth_cost);
    }

    #[test]
    fn simulate_prices_an_open_payer_that_has_not_signed_and_named_senders() {
        let key = signing_key(0x36);
        let sender = eoa_address(&key);
        let tx = TxBase { payer: Some(TxBaseConstants::OPEN_PAYER), ..base_tx() };
        let sender_auth = eoa_sig(&key, tx.sender_signature_hash());
        let signed = TxBaseSigned::new(tx, sender_auth, Bytes::from(vec![0xffu8; 65]));
        let fixture = Fixture::new(sender, eth(), &[]);
        assert!(
            simulate(&mut fixture.evm(), &signed, sender, TxBaseSimulation::Estimate)
                .unwrap()
                .status
        );
        let named = named_signed(TxBase { sender: Some(sender), ..base_tx() }, &key);
        let result =
            simulate(&mut fixture.evm(), &named, sender, TxBaseSimulation::Estimate).unwrap();
        assert!(result.status && result.tx_gas_used() > 0);
    }

    #[test]
    fn underfunded_payer_is_rejected() {
        let key = signing_key(0x33);
        let sender = eoa_address(&key);
        let signed = eoa_signed(base_tx(), &key);
        let mut evm = Fixture::new(sender, U256::from(1_000u64), &[]).evm();
        assert_eq!(transact(&mut evm, &signed).unwrap_err(), HandlerError::InsufficientFunds);
        assert_eq!(info(&mut evm, sender).nonce, 0, "an invalid tx leaves no state");
    }

    #[test]
    fn reverting_call_includes_tx_with_revert_status() {
        let target = address!("0x00000000000000000000000000000000000000c2");
        let key = signing_key(0x55);
        let sender = eoa_address(&key);
        let signed = eoa_signed(TxBase { calls: vec![vec![call(target)]], ..base_tx() }, &key);
        let mut evm = Fixture::new(sender, eth(), &[(target, bytes!("60006000fd"))]).evm();
        let result = transact(&mut evm, &signed).expect("tx is included");
        assert!(!result.status);
        assert_eq!(result.stop, InstrStop::Revert);
        assert_eq!(result.ext.committed_phases, 0);
        assert_eq!(info(&mut evm, sender).nonce, 1);
        assert!(balance(&mut evm, sender) < eth());
    }

    #[test]
    fn call_warms_coinbase_per_eip3651() {
        fn balance_of(address: Address) -> Bytes {
            let mut code = vec![0x73];
            code.extend_from_slice(address.as_slice());
            code.extend_from_slice(&[0x31, 0x00]);
            Bytes::from(code)
        }
        let target = address!("0x00000000000000000000000000000000000000c8");
        let run = |code: Bytes, signer: u8| {
            let key = signing_key(signer);
            let sender = eoa_address(&key);
            let signed = eoa_signed(TxBase { calls: vec![vec![call(target)]], ..base_tx() }, &key);
            let mut evm = Fixture::new(sender, eth(), &[(target, code)]).evm();
            metered_call(&mut evm, &signed, sender).tx_gas_used()
        };
        let warm = run(balance_of(BENEFICIARY), 0x78);
        let cold = run(balance_of(address!("0x00000000000000000000000000000000000000dd")), 0x79);
        assert!(warm < cold);
    }

    #[test]
    fn warmth_is_per_transaction_and_carries_across_committed_phases() {
        let key = signing_key(0x66);
        let sender = eoa_address(&key);
        let loader = address!("0x00000000000000000000000000000000000000c8");
        let fixture = Fixture::new(sender, eth(), &[(loader, warmth_probe_loader_code())]);

        // A first transaction warms (loader, slot 0); the second still pays cold.
        let mut evm = fixture.evm();
        let warm = eoa_signed(TxBase { calls: vec![vec![call(loader)]], ..base_tx() }, &key);
        assert!(transact(&mut evm, &warm).unwrap().status);
        let load = eoa_signed(
            TxBase { nonce_sequence: 1, calls: vec![vec![call(loader)]], ..base_tx() },
            &key,
        );
        assert_eq!(
            metered_call(&mut evm, &load, sender).tx_gas_used(),
            19_423 + COLD_ACCOUNT_ACCESS_COST + WARMTH_PROBE_PAD_GAS
        );

        // An invalid transaction leaks no warmth either.
        let mut evm = fixture.evm();
        let invalid = eoa_signed(
            TxBase { nonce_sequence: 5, calls: vec![vec![call(loader)]], ..base_tx() },
            &key,
        );
        assert!(transact(&mut evm, &invalid).is_err());
        let load = eoa_signed(TxBase { calls: vec![vec![call(loader)]], ..base_tx() }, &key);
        assert_eq!(
            metered_call(&mut evm, &load, sender).tx_gas_used(),
            19_423 + COLD_ACCOUNT_ACCESS_COST + WARMTH_PROBE_PAD_GAS
        );

        // A committed phase leaves the account and slot warm for the next phase.
        let two = eoa_signed(
            TxBase { calls: vec![vec![call(loader)], vec![call(loader)]], ..base_tx() },
            &key,
        );
        assert_eq!(
            metered_call(&mut fixture.evm(), &two, sender).tx_gas_used(),
            19_698 + COLD_ACCOUNT_ACCESS_COST + WARM_ACCOUNT_ACCESS_COST + 2 * WARMTH_PROBE_PAD_GAS
        );
    }

    #[test]
    fn later_phase_skipped_after_earlier_phase_reverts() {
        let reverter = address!("0x00000000000000000000000000000000000000c3");
        let storer = address!("0x00000000000000000000000000000000000000c4");
        let key = signing_key(0x66);
        let sender = eoa_address(&key);
        let signed = eoa_signed(
            TxBase { calls: vec![vec![call(reverter)], vec![call(storer)]], ..base_tx() },
            &key,
        );
        let mut evm = Fixture::new(
            sender,
            eth(),
            &[(reverter, bytes!("60006000fd")), (storer, bytes!("600160005500"))],
        )
        .evm();
        let result = transact(&mut evm, &signed).unwrap();
        assert!(!result.status);
        assert_eq!(result.ext.committed_phases, 0);
        assert_eq!(slot(&mut evm, storer, U256::ZERO), U256::ZERO);
    }

    #[test]
    fn authentication_failures_are_rejected() {
        let account = address!("0x00000000000000000000000000000000000000ca");
        let fixture = Fixture::new(account, eth(), &[]);
        let wrong_key =
            named_signed(TxBase { sender: Some(account), ..base_tx() }, &signing_key(0x99));
        assert!(
            reason(transact(&mut fixture.evm(), &wrong_key).unwrap_err())
                .contains("does not recover to the named account")
        );

        let mut auth = Address::repeat_byte(0x05).to_vec();
        auth.extend_from_slice(&[0u8; 128]);
        let non_k1 = TxBaseSigned::new(
            TxBase { sender: Some(account), ..base_tx() },
            Bytes::from(auth),
            Bytes::new(),
        );
        assert!(
            reason(transact(&mut fixture.evm(), &non_k1).unwrap_err())
                .contains("unsupported authenticator")
        );
        assert!(
            reason(
                simulate(&mut fixture.evm(), &non_k1, account, TxBaseSimulation::Estimate)
                    .unwrap_err()
            )
            .contains("unsupported authenticator")
        );

        let key = signing_key(0x63);
        let padded = TxBaseSigned::new(
            base_tx(),
            eoa_sig(&key, base_tx().sender_signature_hash()),
            Bytes::from_static(&[0xab; 32]),
        );
        assert!(
            reason(transact(&mut fixture.evm(), &padded).unwrap_err())
                .contains("self-paid transaction carries a payer_auth")
        );
    }

    #[test]
    fn base_tx_is_rejected_before_everest() {
        let key = signing_key(0x9c);
        let sender = eoa_address(&key);
        let signed = eoa_signed(base_tx(), &key);
        let mut fixture = Fixture::new(sender, eth(), &[]);
        fixture.upgrade = BaseUpgrade::Denim;
        let err = transact(&mut fixture.evm(), &signed).unwrap_err();
        assert_eq!(err, HandlerError::UnsupportedTransactionType(TxBaseConstants::BASE_TX_TYPE));
        let err =
            simulate(&mut fixture.evm(), &signed, sender, TxBaseSimulation::Call).unwrap_err();
        assert!(reason(err).contains("not active before Everest"));
    }

    #[test]
    fn gas_above_the_per_transaction_cap_is_rejected_and_simulation_is_bounded() {
        let key = signing_key(0x9d);
        let sender = eoa_address(&key);
        let looper = address!("0x00000000000000000000000000000000000000b8");
        let fixture = Fixture::new(sender, U256::MAX >> 1, &[(looper, bytes!("5b600056"))]);
        let cap = fixture.evm().version().tx_gas_limit_cap;
        assert_eq!(cap, 1 << 24);
        let signed = eoa_signed(TxBase { gas_limit: cap + 1, ..base_tx() }, &key);
        assert!(reason(transact(&mut fixture.evm(), &signed).unwrap_err()).contains("EIP-7825"));

        let tx = TxBase {
            gas_limit: cap,
            payer: Some(address!("0x00000000000000000000000000000000000000b7")),
            calls: vec![vec![call(looper)]],
            ..base_tx()
        };
        let mut payer_auth = TxBaseConstants::K1_AUTHENTICATOR.to_vec();
        payer_auth.extend_from_slice(&[0xab; 65]);
        let signed = TxBaseSigned::new(tx, eoa_sig(&key, B256::ZERO), Bytes::from(payer_auth));
        let result =
            simulate(&mut fixture.evm(), &signed, sender, TxBaseSimulation::Estimate).unwrap();
        assert!(!result.status);
        assert!(result.tx_gas_used() <= cap);
    }

    #[test]
    fn nonce_free_with_nonzero_sequence_is_rejected_at_inclusion() {
        let key = signing_key(0x9e);
        let sender = eoa_address(&key);
        let tx = TxBase {
            nonce_key: TxBaseConstants::NONCE_KEY_MAX,
            nonce_sequence: 1,
            valid_before: NOW * 1_000 + 20_000,
            ..base_tx()
        };
        let err = transact(&mut Fixture::new(sender, eth(), &[]).evm(), &eoa_signed(tx, &key))
            .unwrap_err();
        assert!(reason(err).contains("nonce-free transaction has a nonce sequence"));
    }

    #[test]
    fn execution_window_normalizes_seconds_denominated_valid_before() {
        let key = signing_key(0x7a);
        let sender = eoa_address(&key);
        let target = address!("0x00000000000000000000000000000000000000d2");
        let fixture = Fixture::new(sender, eth(), &[(target, bytes!("00"))]);
        let with_before = |valid_before| {
            eoa_signed(TxBase { valid_before, calls: vec![vec![call(target)]], ..base_tx() }, &key)
        };
        assert!(transact(&mut fixture.evm(), &with_before(NOW + 1)).unwrap().status);
        assert!(
            reason(transact(&mut fixture.evm(), &with_before(NOW - 1)).unwrap_err())
                .contains("validity window has expired")
        );
    }

    #[test]
    fn execution_window_and_replay_ring_use_the_base_time_millisecond_clock() {
        const BLOCK_SECONDS: u64 = 1_700_000_000;
        const MILLIS_PART: u64 = 200;
        let now_ms = BLOCK_SECONDS * 1_000 + MILLIS_PART;
        let key = signing_key(0x7b);
        let sender = eoa_address(&key);
        let target = address!("0x00000000000000000000000000000000000000d3");
        let mut fixture = Fixture::new(sender, eth(), &[(target, bytes!("00"))]);
        fixture.storage(Predeploys::BASE_TIME, BASE_TIME_MILLIS_SLOT, U256::from(MILLIS_PART));
        fixture.timestamp = BLOCK_SECONDS;
        let run = |nonce_free: bool, valid_after: u64, valid_before: u64| {
            let mut tx = TxBase { valid_after, valid_before, ..base_tx() };
            tx.calls = vec![vec![call(target)]];
            if nonce_free {
                tx.nonce_key = TxBaseConstants::NONCE_KEY_MAX;
            }
            transact(&mut fixture.evm(), &eoa_signed(tx, &key)).map(|result| result.status)
        };
        for nonce_free in [false, true] {
            let valid_before = now_ms + 20_000;
            assert_eq!(run(nonce_free, now_ms, valid_before), Ok(true));
            assert!(
                reason(run(nonce_free, now_ms + 1, valid_before).unwrap_err())
                    .contains("not yet valid")
            );
            assert!(
                reason(run(nonce_free, 0, now_ms - 1).unwrap_err())
                    .contains("validity window has expired")
            );
        }
        assert_eq!(run(false, 0, now_ms), Ok(true));
        assert_eq!(
            reason(run(true, 0, now_ms).unwrap_err()),
            "transaction validity window has expired"
        );
    }

    #[test]
    fn nonce_free_replay_is_recorded_in_the_ring_and_rejected() {
        let key = signing_key(0x5a);
        let sender = eoa_address(&key);
        // Seconds-denominated (below the milliseconds threshold), normalized to ms.
        let valid_before = NOW + 20;
        let tx = TxBase { nonce_key: TxBaseConstants::NONCE_KEY_MAX, valid_before, ..base_tx() };
        let signed = eoa_signed(tx.clone(), &key);
        let replay_id = tx.replay_id(sender);
        let mut evm = Fixture::new(sender, eth(), &[]).evm();
        assert!(transact(&mut evm, &signed).unwrap().status);
        assert_eq!(info(&mut evm, sender).nonce, 0, "nonce-free leaves the account nonce");
        let nm = NonceManagerSlots::ADDRESS;
        assert_eq!(
            slot(&mut evm, nm, NonceManagerSlots::expiring_nonce_seen_slot(replay_id)),
            U256::from(valid_before * 1_000)
        );
        assert_eq!(
            slot(&mut evm, nm, base_tiger_precompiles::NonceManagerStorage::ring_slot(0)),
            U256::from_be_bytes(replay_id.0)
        );
        assert_eq!(slot(&mut evm, nm, base_tiger_precompiles::NonceManagerStorage::EXPIRING_NONCE_RING_PTR), U256::ONE);
        // A fee-bumped re-signing is the same logical transaction.
        let bumped = eoa_signed(TxBase { max_priority_fee_per_gas: 2_000_000_000, ..tx }, &key);
        assert!(reason(transact(&mut evm, &bumped).unwrap_err()).contains("replay hash"));
    }

    #[test]
    fn cross_call_refunds_do_not_change_the_bill() {
        let target = address!("0x00000000000000000000000000000000000000d1");
        let run = |key_byte: u8, second_store: u8| {
            let key = signing_key(key_byte);
            let sender = eoa_address(&key);
            let mut second = vec![0u8; 32];
            second[31] = second_store;
            let tx = TxBase {
                calls: vec![vec![
                    Call { to: target, value: U256::ZERO, data: Bytes::from(vec![0u8; 32]) },
                    Call { to: target, value: U256::ZERO, data: Bytes::from(second) },
                ]],
                ..base_tx()
            };
            let mut fixture = Fixture::new(sender, eth(), &[(target, bytes!("60003560005500"))]);
            fixture.storage(target, U256::ZERO, U256::ONE);
            let mut evm = fixture.evm();
            let result = transact(&mut evm, &eoa_signed(tx, &key)).unwrap();
            assert!(result.status);
            (result.tx_gas_used(), slot(&mut evm, target, U256::ZERO))
        };
        let (restore_gas, restore_slot) = run(0x99, 1);
        let (reclear_gas, reclear_slot) = run(0x9a, 0);
        assert_eq!(restore_slot, U256::ONE);
        assert_eq!(reclear_slot, U256::ZERO);
        assert_eq!(restore_gas, base_tx().gas_limit);
        assert_eq!(reclear_gas, restore_gas);
    }

    fn call_pool_gas_with_coinbase(
        signed: &TxBaseSigned,
        sender: Address,
        accounts: &[(Address, Bytes)],
        coinbase: Address,
    ) -> u64 {
        let mut fixture = Fixture::new(sender, eth(), &[(FLOOR_BURNER, floor_burner_code())]);
        for (address, code) in accounts {
            fixture.code(*address, code.clone());
        }
        fixture.beneficiary = coinbase;
        let intrinsic = IntrinsicGas::compute(signed, sender);
        let metered = metered_call(&mut fixture.evm(), signed, sender);
        assert!(metered.status);
        assert!(metered.tx_gas_used() > intrinsic.sender_floor());
        metered.tx_gas_used() - intrinsic.sender_intrinsic()
    }

    fn signed_root_calls(key: &SigningKey, targets: &[Address]) -> TxBaseSigned {
        let calls =
            core::iter::once(FLOOR_BURNER).chain(targets.iter().copied()).map(call).collect();
        eoa_signed(TxBase { calls: vec![calls], ..base_tx() }, key)
    }

    #[test]
    fn root_call_access_gas_follows_eip2929() {
        let target = address!("0x00000000000000000000000000000000000000cc");
        let delegate = address!("0x00000000000000000000000000000000000000cd");
        let key = signing_key(0xe1);
        let sender = eoa_address(&key);
        let one = signed_root_calls(&key, &[target]);
        let two = signed_root_calls(&key, &[target, target]);
        let pad = WARMTH_PROBE_PAD_GAS;
        let (cold, warm) = (COLD_ACCOUNT_ACCESS_COST, WARM_ACCOUNT_ACCESS_COST);
        assert_eq!(call_pool_gas_with_coinbase(&one, sender, &[], BENEFICIARY), pad + cold);
        assert_eq!(call_pool_gas_with_coinbase(&one, sender, &[], target), pad + warm);
        assert_eq!(call_pool_gas_with_coinbase(&two, sender, &[], BENEFICIARY), pad + cold + warm);

        let accounts = [(target, Bytecode::new_eip7702(delegate).original_bytes())];
        assert_eq!(
            call_pool_gas_with_coinbase(&one, sender, &accounts, BENEFICIARY),
            pad + 2 * cold
        );
        assert_eq!(
            call_pool_gas_with_coinbase(&one, sender, &accounts, delegate),
            pad + cold + warm
        );
        assert_eq!(call_pool_gas_with_coinbase(&one, sender, &accounts, target), pad + cold + warm);
        assert_eq!(
            call_pool_gas_with_coinbase(&two, sender, &accounts, BENEFICIARY),
            pad + 2 * cold + 2 * warm
        );
    }

    #[test]
    fn simulate_estimate_includes_root_call_access() {
        let key = signing_key(0xe4);
        let sender = eoa_address(&key);
        let target = address!("0x00000000000000000000000000000000000000cc");
        let signed = signed_root_calls(&key, &[target]);
        let gas = |coinbase: Address, mode| {
            let mut fixture = Fixture::new(sender, eth(), &[(FLOOR_BURNER, floor_burner_code())]);
            fixture.beneficiary = coinbase;
            simulate(&mut fixture.evm(), &signed, sender, mode).unwrap().tx_gas_used()
        };
        let cold = gas(BENEFICIARY, TxBaseSimulation::Estimate);
        let warm = gas(target, TxBaseSimulation::Estimate);
        assert_eq!(cold - warm, COLD_ACCOUNT_ACCESS_COST - WARM_ACCOUNT_ACCESS_COST);
        assert_eq!(cold, gas(BENEFICIARY, TxBaseSimulation::Call));
        assert_eq!(warm, gas(target, TxBaseSimulation::Call));
    }

    #[test]
    fn halting_and_reverting_root_calls() {
        let target = address!("0x00000000000000000000000000000000000000f1");
        let run = |signer: u8, code: Bytes| {
            let key = signing_key(signer);
            let sender = eoa_address(&key);
            let signed = eoa_signed(TxBase { calls: vec![vec![call(target)]], ..base_tx() }, &key);
            let fixture = Fixture::new(sender, eth(), &[(target, code)]);
            let result = transact(&mut fixture.evm(), &signed).unwrap();
            assert!(!result.status);
            let estimate =
                simulate(&mut fixture.evm(), &signed, sender, TxBaseSimulation::Estimate).unwrap();
            assert!(!estimate.status);
            (result.tx_gas_used(), estimate.tx_gas_used(), signed.tx().gas_limit)
        };
        let (used, estimate, limit) = run(0xd1, bytes!("fe"));
        assert_eq!((used, estimate), (limit, limit), "a halt burns the whole pool");
        let (used, estimate, limit) = run(0xd2, bytes!("60006000fd"));
        assert_eq!(used, limit);
        assert!(estimate < limit, "a revert keeps its unused gas");
    }

    #[test]
    fn halting_root_call_keeps_earlier_phases_and_skips_later_ones() {
        let key = signing_key(0xd3);
        let sender = eoa_address(&key);
        let first = address!("0x00000000000000000000000000000000000000f2");
        let halter = address!("0x00000000000000000000000000000000000000f3");
        let last = address!("0x00000000000000000000000000000000000000f4");
        let signed = eoa_signed(
            TxBase {
                calls: vec![vec![call(first)], vec![call(halter)], vec![call(last)]],
                ..base_tx()
            },
            &key,
        );
        let storer = bytes!("600160005500");
        let mut evm = Fixture::new(
            sender,
            eth(),
            &[(first, storer.clone()), (halter, bytes!("fe")), (last, storer)],
        )
        .evm();
        let result = transact(&mut evm, &signed).unwrap();
        assert!(!result.status);
        assert_eq!(result.tx_gas_used(), signed.tx().gas_limit);
        assert_eq!(result.ext.committed_phases, 1);
        assert_eq!(slot(&mut evm, first, U256::ZERO), U256::ONE);
        assert_eq!(slot(&mut evm, last, U256::ZERO), U256::ZERO);
    }

    #[test]
    fn root_call_starts_with_empty_memory() {
        let key = signing_key(0xd4);
        let sender = eoa_address(&key);
        let writer = address!("0x00000000000000000000000000000000000000f5");
        let reader = address!("0x00000000000000000000000000000000000000f6");
        let writer_code = bytes!(
            "7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff5f527fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff60605260005ff3"
        );
        let reader_code = bytes!("595f555a5f515a90600155900360025500");
        let signed =
            eoa_signed(TxBase { calls: vec![vec![call(writer), call(reader)]], ..base_tx() }, &key);
        let mut evm =
            Fixture::new(sender, eth(), &[(writer, writer_code), (reader, reader_code)]).evm();
        assert!(transact(&mut evm, &signed).unwrap().status);
        assert_eq!(slot(&mut evm, reader, U256::ZERO), U256::ZERO, "MSIZE at entry");
        assert_eq!(slot(&mut evm, reader, U256::ONE), U256::ZERO, "MLOAD(0) at entry");
        assert_eq!(slot(&mut evm, reader, U256::from(2)), U256::from(2 + 3 + 3 + 2));
    }

    #[test]
    fn simulate_checks_intrinsic_gas_before_installing_the_delegation() {
        let key = signing_key(0xc9);
        let sender = eoa_address(&key);
        let delegation = AccountChange::Delegation(Delegation {
            target: address!("0x00000000000000000000000000000000000000dd"),
        });
        let tx = TxBase {
            sender: Some(sender),
            account_changes: vec![delegation.clone(), delegation],
            gas_limit: 1,
            ..base_tx()
        };
        let signed = named_signed(tx, &key);
        let err = simulate(
            &mut Fixture::new(sender, eth(), &[]).evm(),
            &signed,
            sender,
            TxBaseSimulation::Estimate,
        )
        .unwrap_err();
        assert!(reason(err).contains("below the EIP-7623 calldata floor"));
    }

    /// The block executor reserves `gas_limit + payer_auth_cost` for a Base transaction, and its
    /// receipt is the `0x79` receipt with the committed phase count and the committed phase's
    /// logs. Ported from the revm block executor tests.
    #[test]
    fn block_executor_builds_the_base_receipt_and_reserves_payer_auth() {
        use base_common_consensus::BaseReceiptEnvelope;

        use crate::{BaseBlockExecutionCtx, BaseBlockExecutor, BlockGasLimitExceeded};

        let key = signing_key(0x41);
        let sender = eoa_address(&key);
        let logger = address!("0x00000000000000000000000000000000000000a1");
        let reverter = address!("0x00000000000000000000000000000000000000a2");
        // PUSH1 0 PUSH1 0 LOG0 STOP.
        let fixture = Fixture::new(
            sender,
            eth(),
            &[(logger, bytes!("60006000a000")), (reverter, bytes!("60006000fd"))],
        );
        let signed = eoa_signed(
            TxBase { calls: vec![vec![call(logger)], vec![call(reverter)]], ..base_tx() },
            &key,
        );
        let mut executor = BaseBlockExecutor::new(fixture.evm(), BaseBlockExecutionCtx::default());
        executor.execute_transaction(&envelope(&signed)).expect("tx is included");
        let (_, result, _) = executor.finish();
        assert_eq!(result.gas_used, signed.tx().gas_limit);
        let [BaseReceiptEnvelope::Base(receipt)] = result.receipts.as_slice() else {
            panic!("expected one Base receipt, got {:?}", result.receipts);
        };
        assert!(!receipt.receipt.inner.status.coerce_status());
        assert_eq!(receipt.receipt.committed_phases, 1);
        assert_eq!(receipt.receipt.inner.cumulative_gas_used, signed.tx().gas_limit);
        assert_eq!(receipt.receipt.inner.logs.len(), 1, "the committed phase's log survives");
        assert_eq!(receipt.receipt.inner.logs[0].address, logger);
        receipt.receipt.validate_committed_phases(2).expect("receipt invariant");

        // A sponsored transaction whose gas limit fits the block only without payer
        // authentication is rejected.
        let payer_key = signing_key(0x42);
        let payer = eoa_address(&payer_key);
        let block_gas_limit = 60_000_000;
        let tx = TxBase {
            gas_limit: block_gas_limit - 1,
            payer: Some(TxBaseConstants::OPEN_PAYER),
            ..base_tx()
        };
        let sponsored = open_payer_signed(tx, &key, &payer_key);
        let mut executor = BaseBlockExecutor::new(
            Fixture::new(payer, eth(), &[]).evm(),
            BaseBlockExecutionCtx::default(),
        );
        let err = executor.execute_transaction(&envelope(&sponsored)).unwrap_err();
        let expected = BlockGasLimitExceeded {
            transaction_gas_limit: block_gas_limit - 1 + TxBaseGasSchedule::K1_AUTH_COST,
            block_available_gas: block_gas_limit,
        };
        assert_eq!(err.to_string(), expected.to_string());
    }

    /// Gas-limit billing applies to standard transactions too: from Everest an EIP-1559 transfer
    /// reports and pays for its whole gas limit; before it, the 21,000 it metered.
    #[test]
    fn gas_limit_billing_bills_a_standard_transaction_its_gas_limit() {
        use alloy_consensus::TxEip1559;
        use alloy_primitives::TxKind;
        use evm2::ethereum::TxEnvelope;

        let sender = address!("0x00000000000000000000000000000000000000a5");
        let recipient = address!("0x00000000000000000000000000000000000000a6");
        let gas_limit = 100_000;
        let tx = TxEip1559 {
            chain_id: CHAIN_ID,
            nonce: 0,
            gas_limit,
            max_fee_per_gas: 5_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to: TxKind::Call(recipient),
            value: U256::from(7),
            ..Default::default()
        };
        let envelope = Recovered::new_unchecked(
            BaseTxEnvelope::standard(TxEnvelope::Eip1559(tx), Bytes::from_static(&[0x02, 0xc0])),
            sender,
        );
        for (upgrade, expected) in [(BaseUpgrade::Denim, 21_000), (BaseUpgrade::Everest, gas_limit)]
        {
            let mut fixture = Fixture::new(sender, eth(), &[]);
            fixture.upgrade = upgrade;
            let mut evm = fixture.evm();
            let result = evm.transact(&envelope).unwrap().commit();
            assert!(result.status);
            assert_eq!(result.tx_gas_used(), expected, "{upgrade:?}");
            let paid = eth() - balance(&mut evm, sender) - U256::from(7);
            assert_eq!(paid, U256::from(expected) * U256::from(2_000_000_000u64), "{upgrade:?}");
            assert_eq!(
                balance(&mut evm, BENEFICIARY),
                U256::from(expected) * U256::from(1_000_000_000u64)
            );
        }
    }
}
