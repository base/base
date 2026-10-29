//! Engine-neutral EIP-8130 execution pipeline.
//!
//! The storage-backed pre-call pipeline (authorization, account changes, nonce, intrinsic gas,
//! fee checks, and the transaction-context publish), its read-only simulation variant, and the
//! gas-settlement arithmetic. Every step runs over a [`StorageCtx`], so the revm and EVM2 engines
//! execute the identical code against their own gas-free journal providers.

use core::fmt::Display;

use alloy_primitives::{Address, B256, Bytes, U256, address};
use base_common_consensus::{
    AccountChange, Delegation, Eip8130Constants, Eip8130Signed, TxEip8130,
};
use base_common_precompiles::{NonceManagerStorage, TxContextStorage};
use base_precompile_storage::{Bytecode, StorageCtx};

use crate::{
    AccountChangeApplier, AccountConfigurationStorage, ApplyError, DelegationEffect, FeeCheck,
    FeeError, IntrinsicGas, IntrinsicGasInput, NonceMode, NonceValidator, TransactionAuthorizer,
};

/// EIP-3529 maximum gas refund quotient: refunds are capped at `gas_used / 5`.
/// Base is post-London, so this is constant across all live specs.
const MAX_REFUND_QUOTIENT: u64 = 5;

/// A rejected EIP-8130 transaction: the transaction is invalid and must not be included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Eip8130Error {
    /// The transaction fails an EIP-8130 validity rule.
    Invalid(String),
    /// The gas payer cannot cover the worst-case fee. Kept distinct so engines can report it as
    /// their standard "lack of funds for max fee" error, which block builders skip rather than
    /// abort on: a payer's balance can change after admission, including earlier in the block.
    PayerCannotPay {
        /// The payer's balance.
        balance: U256,
        /// The worst-case amount the payer must cover.
        required: U256,
    },
}

impl Eip8130Error {
    /// Builds an [`Eip8130Error::Invalid`] from any displayable rejection reason.
    pub fn invalid(reason: impl Display) -> Self {
        Self::Invalid(reason.to_string())
    }
}

impl Display for Eip8130Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Invalid(reason) => f.write_str(reason),
            Self::PayerCannotPay { balance, required } => {
                write!(f, "payer balance {balance} cannot cover the worst-case fee {required}")
            }
        }
    }
}

impl core::error::Error for Eip8130Error {}

/// Block and chain inputs of the EIP-8130 pre-call pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Eip8130BlockContext {
    /// The local chain id.
    pub chain_id: u64,
    /// The block timestamp in Unix seconds (the clock of `ActorConfig::expiry`).
    pub now: u64,
    /// The block timestamp in Unix milliseconds (the clock of the validity window and the
    /// nonce-free replay ring).
    pub now_ms: u64,
    /// The block base fee per gas.
    pub base_fee: u128,
    /// The EIP-7825 per-transaction gas cap.
    pub tx_gas_limit_cap: u64,
    /// Whether the Keystore (`AccountConfiguration`) is active (Zenith onwards).
    pub keystore: bool,
}

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
    /// EIP-7623 sender-side calldata floor: the minimum sender gas the
    /// transaction is metered regardless of how little its `calls` execute
    /// (`>= sender_intrinsic`). `gas_limit` is guaranteed `>= sender_floor` (the
    /// transaction is rejected otherwise), and settlement charges at least this
    /// for the sender portion.
    pub sender_floor: u64,
    /// Payer-authentication gas with the `payer_auth` bytes at the EIP-7623
    /// floor rate (`payer_floor`), metered on top of `gas_limit`. The ceiling
    /// used for the pre-charge and the EIP-7825 cap.
    pub payer_auth: u64,
    /// Payer-authentication gas with the bytes at the standard rate
    /// (`payer_auth_cost`), charged at settlement unless the floor binds.
    pub payer_auth_standard: u64,
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

impl Eip8130Outcome {
    /// Applies the EIP-7623 floor to the sender portion of metered gas.
    ///
    /// Every path that reports or charges sender gas goes through this, so the
    /// floor is applied the same way in estimation and settlement.
    #[must_use]
    pub const fn floored_sender_gas(&self, sender_gas: u64) -> u64 {
        if sender_gas < self.sender_floor { self.sender_floor } else { sender_gas }
    }

    /// Total metered gas for `sender_gas` of sender work, payer authentication
    /// included: the standard charge or the EIP-7623 floor, whichever is larger,
    ///
    /// ```text
    /// max(sender_gas + payer_auth_cost, sender_floor + payer_floor)
    /// ```
    #[must_use]
    pub const fn metered_gas(&self, sender_gas: u64) -> u64 {
        let standard = sender_gas.saturating_add(self.payer_auth_standard);
        let floor = self.sender_floor.saturating_add(self.payer_auth);
        if standard < floor { floor } else { standard }
    }

    /// Worst-case chargeable gas: the full sender budget plus payer authentication, both billed
    /// at the effective price. This is what the payer is pre-charged for.
    #[must_use]
    pub const fn max_chargeable_gas(&self) -> u64 {
        FeeCheck::max_chargeable_gas(self.gas_limit, self.payer_auth)
    }

    /// The gas billed for an EIP-8130 transaction whose `calls` consumed `call_gas_spent` and
    /// accumulated the signed transaction-level `refund`: sender-intrinsic plus call gas, less
    /// the EIP-3529-capped refund, plus payer authentication (billed on top of the sender
    /// budget).
    ///
    /// The refund-cap denominator (`gross_used`) includes `sender_intrinsic` —
    /// like mainnet, where intrinsic gas counts toward the `gas_used / 5` ceiling —
    /// but deliberately excludes `payer_auth`: payer authentication carries no
    /// SSTORE/SELFDESTRUCT refund of its own, so it must not inflate the refund
    /// ceiling. (Mainnet has no payer-auth concept, so this is an EIP-8130-specific
    /// choice rather than literal mainnet parity.)
    ///
    /// This is the **net consensus charge**. The read-only estimate uses the gross amount
    /// instead — `sender_intrinsic + call_gas_spent + payer_auth` — because refunds are
    /// credited after execution and are never available to the call pool during
    /// execution, so the gas limit must cover the full gross spend.
    ///
    /// The EIP-7623 floor applies to the total ([`Self::metered_gas`]):
    /// a data-heavy transaction whose `calls` execute cheaply still pays
    /// `sender_floor + payer_floor`. Mirrors revm's `eip7623_check_gas_floor`,
    /// which compares the floor against the post-refund gas used.
    #[must_use]
    pub fn billable_gas(&self, call_gas_spent: u64, refund: i64) -> u64 {
        let gross_used = self.sender_intrinsic.saturating_add(call_gas_spent);
        let refund = Eip8130Pipeline::capped_refund(refund, gross_used);
        self.metered_gas(gross_used.saturating_sub(refund))
    }
}

/// The fee amounts an EIP-8130 transaction settles, given the billed gas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Eip8130FeeSplit {
    /// The execution fee (`billable_gas * effective_gas_price`).
    pub fee: U256,
    /// The base-fee portion of the execution fee (routed to the base-fee vault).
    pub base_fee_amount: U256,
    /// The priority-fee portion of the execution fee (routed to the block beneficiary).
    pub priority_amount: U256,
}

impl Eip8130FeeSplit {
    /// Splits the execution fee for `billable_gas` at the outcome's effective price and base fee.
    pub fn new(outcome: &Eip8130Outcome, billable_gas: u64) -> Result<Self, Eip8130Error> {
        let fee = U256::from(billable_gas)
            .checked_mul(U256::from(outcome.effective))
            .ok_or_else(|| Eip8130Error::invalid("EIP-8130 fee overflow"))?;
        let base_fee_amount = U256::from(billable_gas)
            .checked_mul(U256::from(outcome.base_fee))
            .ok_or_else(|| Eip8130Error::invalid("EIP-8130 base-fee amount overflow"))?;
        let priority_amount = fee
            .checked_sub(base_fee_amount)
            .ok_or_else(|| Eip8130Error::invalid("EIP-8130 priority amount underflow"))?;
        Ok(Self { fee, base_fee_amount, priority_amount })
    }
}

/// The engine-neutral EIP-8130 execution pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Eip8130Pipeline;

impl Eip8130Pipeline {
    /// Payer published to the `TxContext` precompile when simulating an open
    /// payer transaction before any payer has signed (its `payer_auth` does not
    /// recover). The real payer is unknown, and the context cannot carry the
    /// zero address, which would read back as `tx.origin` (the sender).
    pub const UNSIGNED_OPEN_PAYER: Address = address!("0x0000000000000000000000000000000000008130");

    /// The rejection for an EIP-8130 transaction under a spec before Everest.
    pub fn not_active_error() -> Eip8130Error {
        Eip8130Error::invalid("EIP-8130 transactions are not active before Everest")
    }

    /// Converts the block timestamp to Unix seconds, rejecting rather than saturating a value
    /// that does not fit: a clamped timestamp would silently shift the expiry validation in the
    /// authorizer and nonce validator. Block timestamps never approach `u64::MAX` in practice.
    pub fn block_timestamp(timestamp: U256) -> Result<u64, Eip8130Error> {
        timestamp.try_into().map_err(|_| Eip8130Error::invalid("block timestamp exceeds u64"))
    }

    /// Consensus-critical cross-chain-replay guard. The sender and payer
    /// signature hashes commit to the transaction body's *embedded* `chain_id`,
    /// not the local chain, so a signature produced for another chain verifies
    /// bit-for-bit when the identical bytes are executed here. Pool admission
    /// rejects a chain-id mismatch, but block inclusion bypasses the pool, so the
    /// equality MUST be enforced by the execution pipeline every inclusion path shares.
    pub fn check_chain_id(tx: &TxEip8130, chain_id: u64) -> Result<(), Eip8130Error> {
        if tx.chain_id != chain_id {
            return Err(Eip8130Error::invalid("chain id mismatch"));
        }
        Ok(())
    }

    /// Consensus-level validity window. The transaction is includable only
    /// within the inclusive interval `[valid_after, valid_before]` on the
    /// millisecond axis (`now_ms`, the block's millisecond timestamp). Per the
    /// EIP a transaction is still valid at `now_ms == valid_before`, so the
    /// upper bound is inclusive: reject only once `now_ms` is strictly past
    /// `valid_before`.
    ///
    /// Validity bounds are normalized to Unix milliseconds (seconds bounds are
    /// scaled by 1000 per EIP-8130 Timestamp Normalization); `0` disables the
    /// respective bound. Both bounds apply to nonce-free and nonce-bearing
    /// transactions alike. The nonce-free replay ring separately enforces its own
    /// admission window (`valid_before > now_ms`) when it records the nonce, so a
    /// nonce-free transaction at the boundary still fails there.
    pub fn check_validity_window(tx: &TxEip8130, now_ms: u64) -> Result<(), Eip8130Error> {
        let valid_after = tx.valid_after_ms();
        let valid_before = tx.valid_before_ms();
        if valid_after != 0 && now_ms < valid_after {
            return Err(Eip8130Error::invalid("transaction is not yet valid"));
        }
        if valid_before != 0 && now_ms > valid_before {
            return Err(Eip8130Error::invalid("transaction validity window has expired"));
        }
        Ok(())
    }

    /// Runs the storage-backed pre-call pipeline (authorize, nonce, intrinsic
    /// gas, fee-cap check, account-change apply) and publishes the transaction
    /// context, returning the resolved [`Eip8130Outcome`]. Storage writes land on
    /// `sctx` directly; the caller MUST discard the transaction on error.
    ///
    /// `sctx` must be a gas-free journal view: the EIP-8130 schedule is the single
    /// source of gas accounting for the pre-call work. The validity window is checked
    /// separately by [`Self::check_validity_window`].
    pub fn authorize_and_apply(
        sctx: StorageCtx<'_>,
        signed: &Eip8130Signed,
        encoded: &[u8],
        block: &Eip8130BlockContext,
    ) -> Result<Eip8130Outcome, Eip8130Error> {
        let tx = signed.tx();
        let nonce_key = tx.nonce_key;
        let gas_limit = tx.gas_limit;
        let max_fee = tx.max_fee_per_gas;
        let max_priority = tx.max_priority_fee_per_gas;
        // The nonce-free replay ring records this normalized upper bound and
        // compares it against `now_ms`, so it MUST be the normalized value. The raw
        // `valid_before` remains the signed/encoded/replay-committed field.
        let valid_before = tx.valid_before_ms();

        // Ordering note: the apply step (1) and code effects (2) write journal
        // storage *before* the nonce is validated (3). Any `Err` returned here
        // makes the caller discard the transaction, so these earlier writes never
        // persist for a rejected transaction. This mirrors the caller-MUST-discard
        // contract documented on `TransactionAuthorizer::authorize_and_apply`.
        let mut acc = AccountConfigurationStorage::new(sctx);

        // 1. Authorize and apply the account changes interleaved against the
        //    evolving state, then authenticate sender/payer against the resulting
        //    post-apply state. Before Zenith there is no Keystore: authorization
        //    is pure secp256k1 recovery and never reads `AccountConfiguration`.
        let applied_tx = if block.keystore {
            TransactionAuthorizer::authorize_and_apply(signed, &mut acc, block.chain_id, block.now)
        } else {
            TransactionAuthorizer::authorize_without_keystore(signed)
        }
        .map_err(Eip8130Error::invalid)?;
        let sender_actor = applied_tx.actors.sender.resolved;
        let sender = applied_tx.actors.sender.account;
        let payer = applied_tx.actors.payer.as_ref().map_or(sender, |p| p.account);
        // Defense-in-depth: `verify_sender` already gates `can_use_nonce_key`; kept
        // so this entry point stays sound if sender resolution is ever refactored.
        if !sender_actor.can_use_nonce_key(nonce_key) {
            return Err(Eip8130Error::invalid(
                "sender actor scope does not authorize sequenced nonces",
            ));
        }

        // 2. Install the deferred account-*code* effects (created-account
        //    bytecode, delegation indicator) the apply step surfaced.
        if let Some(created) = &applied_tx.applied.created {
            Self::install_created_code(sctx, created.address, &created.code)?;
        }
        if let Some(delegation) = &applied_tx.applied.delegation {
            delegation.install(sctx).map_err(Eip8130Error::invalid)?;
        }

        // 3. Validate and advance the nonce. The nonce-free replay lookup works in
        //    milliseconds (`now_ms`), matching the validity window and the ring
        //    buffer; the sequence-channel branches ignore this argument.
        let mut nonce_mgr = NonceManagerStorage::new(sctx);
        let protocol_nonce =
            sctx.with_account_info(sender, |info| Ok(info.nonce)).map_err(Eip8130Error::invalid)?;
        let (nonce_key_first_use, bump_protocol_nonce) =
            if nonce_key == Eip8130Constants::NONCE_KEY_MAX {
                NonceValidator::validate(
                    tx,
                    sender,
                    protocol_nonce,
                    &nonce_mgr,
                    NonceMode::Inclusion,
                    block.now_ms,
                )
                .map_err(Eip8130Error::invalid)?;
                let replay = NonceValidator::replay_hash(tx, sender);
                nonce_mgr
                    .check_and_mark_expiring_nonce(replay, valid_before, block.now_ms)
                    .map_err(Eip8130Error::invalid)?;
                (false, false)
            } else if nonce_key == U256::ZERO {
                NonceValidator::validate(
                    tx,
                    sender,
                    protocol_nonce,
                    &nonce_mgr,
                    NonceMode::Inclusion,
                    block.now_ms,
                )
                .map_err(Eip8130Error::invalid)?;
                (protocol_nonce == 0, true)
            } else {
                let current_nonce =
                    nonce_mgr.get_nonce(sender, nonce_key).map_err(Eip8130Error::invalid)?;
                NonceValidator::validate_sequence(tx, current_nonce, NonceMode::Inclusion)
                    .map_err(Eip8130Error::invalid)?;
                nonce_mgr.increment_nonce(sender, nonce_key).map_err(Eip8130Error::invalid)?;
                (current_nonce == 0, false)
            };

        // 4. Intrinsic gas under the EIP-8130 schedule.
        let (intrinsic, execution_gas_available) = Self::resolve_execution_gas(
            signed,
            encoded,
            &IntrinsicGasInput::new(sender, nonce_key_first_use)
                .with_sender_policy_gate(sender_actor.is_policy_gated())
                .with_revoke_discount_slots(applied_tx.revoke_discount_slots),
            gas_limit,
        )?;
        // EIP-7825: the transaction may consume `gas_limit` plus the payer
        // authentication metered on top of it, and that total is bounded by the
        // per-transaction gas cap (from Azul).
        if FeeCheck::max_chargeable_gas(gas_limit, intrinsic.payer_auth) > block.tx_gas_limit_cap {
            return Err(Eip8130Error::invalid(
                "transaction gas exceeds the EIP-7825 per-transaction cap",
            ));
        }

        // 5. Fee caps and payer balance.
        FeeCheck::validate_fees(max_fee, max_priority, block.base_fee)
            .map_err(Eip8130Error::invalid)?;
        let payer_balance = sctx
            .with_account_info(payer, |info| Ok(info.balance))
            .map_err(Eip8130Error::invalid)?;
        FeeCheck::validate_balance(payer_balance, gas_limit, intrinsic.payer_auth, max_fee)
            .map_err(|error| match error {
                FeeError::InsufficientBalance { balance, required } => {
                    Eip8130Error::PayerCannotPay { balance, required }
                }
                error => Eip8130Error::invalid(error),
            })?;

        // 6. Publish the transaction context (sender / payer / actor id) so it is
        //    readable by the `TxContext` precompile during `calls`.
        TxContextStorage::new(sctx)
            .set_context(sender, payer, sender_actor.actor_id)
            .map_err(Eip8130Error::invalid)?;

        Ok(Eip8130Outcome {
            sender,
            payer,
            sender_actor_id: sender_actor.actor_id,
            policy_gated: sender_actor.is_policy_gated(),
            policy_target: sender_actor.policy_target,
            gas_limit,
            sender_intrinsic: intrinsic.sender_intrinsic(),
            sender_floor: intrinsic.sender_floor(),
            payer_auth: intrinsic.payer_auth,
            payer_auth_standard: intrinsic.payer_auth_standard,
            execution_gas_available,
            effective: FeeCheck::effective_gas_price(max_fee, max_priority, block.base_fee),
            base_fee: block.base_fee,
            bump_protocol_nonce,
        })
    }

    /// Resolves the [`Eip8130Outcome`] for read-only simulation (`eth_call` /
    /// `eth_estimateGas`): prices intrinsic gas and checks it against the gas
    /// limit, then applies account changes, then resolves the acting actor (an
    /// optional RPC hint, else the account's self-actor) and its policy from the
    /// post-apply state — no signature recovery — without validating or advancing
    /// the nonce or checking the payer balance. The authentication gas for the
    /// sender's (and any payer's) declared authenticator is priced from the
    /// synthesized auth-blob shape via [`IntrinsicGas`]. Storage writes land on
    /// `sctx`; the caller reverts them.
    ///
    /// `acting_actor_hint` is the optional `senderActorId` from the estimate
    /// request. Without it, simulation publishes the self-actor.
    pub fn simulate_resolve(
        sctx: StorageCtx<'_>,
        signed: &Eip8130Signed,
        encoded: &[u8],
        sender: Address,
        acting_actor_hint: Option<B256>,
        block: &Eip8130BlockContext,
    ) -> Result<Eip8130Outcome, Eip8130Error> {
        let tx = signed.tx();
        let nonce_key = tx.nonce_key;
        let keystore = block.keystore;
        // EIP-7825: execution bounds `gas_limit` plus payer authentication by the
        // per-transaction cap, so simulate at most the gas a real transaction could
        // carry. The payer authentication ceiling is the worst case, so a
        // submission at the returned estimate always fits.
        let payer_auth_ceiling =
            IntrinsicGas::max_payer_auth_cost(signed).map_err(Eip8130Error::invalid)?;
        let gas_limit = tx.gas_limit.min(block.tx_gas_limit_cap.saturating_sub(payer_auth_ceiling));
        let max_fee = tx.max_fee_per_gas;
        let max_priority = tx.max_priority_fee_per_gas;
        // Use the declared payer (sponsor) so the payer published to the
        // `TxContext` precompile matches a real execution. A named payer's
        // signature is not verified here. An open payer is the signer its
        // `payer_auth` recovers to; before a payer has signed the payer is unknown
        // and published as `UNSIGNED_OPEN_PAYER`, never the sender.
        let payer = signed.resolved_payer(sender).unwrap_or(Self::UNSIGNED_OPEN_PAYER);
        if !keystore {
            TransactionAuthorizer::check_without_keystore(signed).map_err(Eip8130Error::invalid)?;
        }

        let nonce_mgr = NonceManagerStorage::new(sctx);

        // 1. Nonce-channel first-use flag (drives intrinsic gas). Estimation
        //    neither validates nor advances the nonce.
        let protocol_nonce =
            sctx.with_account_info(sender, |info| Ok(info.nonce)).map_err(Eip8130Error::invalid)?;
        let nonce_key_first_use = if nonce_key == Eip8130Constants::NONCE_KEY_MAX {
            false
        } else if nonce_key == U256::ZERO {
            protocol_nonce == 0
        } else {
            nonce_mgr.get_nonce(sender, nonce_key).map_err(Eip8130Error::invalid)? == 0
        };

        // 2. Intrinsic gas, a safe ceiling execution can only meet or undercharge:
        //    the non-monotonic, state-dependent costs are pinned to their worst
        //    case (the policy gate charged from Zenith, zero revoke discount) while
        //    the monotonic nonce first-use cost stays resolved. None of this depends
        //    on the applied account changes, so a `gas_limit` that cannot cover it
        //    is rejected before any change is applied.
        let (intrinsic, execution_gas_available) = Self::resolve_execution_gas(
            signed,
            encoded,
            &IntrinsicGasInput::worst_case(sender, nonce_key_first_use, keystore),
            gas_limit,
        )?;

        // 3. Apply account changes and install deferred code effects so the calls
        //    run against post-change code. Must precede actor/policy resolution so
        //    an actor authorized in this same request is visible.
        Self::apply_account_changes(signed, sctx, sender, block.now)?;

        // 4. Resolve the acting actor's real policy gate from the post-apply state.
        //    This drives only call-gating, never the intrinsic estimate (which pins
        //    the gate worst-case). Before Zenith the sender is always its own
        //    ungated self-actor.
        let sender_actor_id = acting_actor_hint
            .filter(|_| keystore)
            .unwrap_or_else(|| AccountConfigurationStorage::self_actor_id(sender));
        let (policy_gated, policy_target) = if keystore {
            Self::simulate_policy_gate(sctx, sender, sender_actor_id)?
        } else {
            (false, Address::ZERO)
        };

        // 5. Publish the transaction context for the `TxContext` precompile.
        TxContextStorage::new(sctx)
            .set_context(sender, payer, sender_actor_id)
            .map_err(Eip8130Error::invalid)?;

        Ok(Eip8130Outcome {
            sender,
            payer,
            sender_actor_id,
            policy_gated,
            policy_target,
            gas_limit,
            sender_intrinsic: intrinsic.sender_intrinsic(),
            sender_floor: intrinsic.sender_floor(),
            payer_auth: intrinsic.payer_auth,
            payer_auth_standard: intrinsic.payer_auth_standard,
            execution_gas_available,
            effective: FeeCheck::effective_gas_price(max_fee, max_priority, block.base_fee),
            base_fee: block.base_fee,
            bump_protocol_nonce: false,
        })
    }

    /// Resolves the simulated acting actor's policy gate from the Keystore:
    /// whether it is policy-gated and, if so, its policy target.
    ///
    /// The acting scope comes from the effective-config resolver (an explicit
    /// `actor_config` entry, or the inline secp256k1 self; a revoked default EOA
    /// resolves to scope 0). The policy target is read with `get_policy_manager`
    /// only when gated, avoiding `get_policy`'s unused `policy_commitment` SLOAD.
    pub fn simulate_policy_gate(
        sctx: StorageCtx<'_>,
        sender: Address,
        sender_actor_id: B256,
    ) -> Result<(bool, Address), Eip8130Error> {
        let acc = AccountConfigurationStorage::new(sctx);
        let actor_scope =
            acc.resolve_actor_config(sender, sender_actor_id).map_err(Eip8130Error::invalid)?.scope;
        let policy_gated = Eip8130Constants::sender_is_policy_gated(actor_scope);
        let policy_target = if policy_gated {
            acc.get_policy_manager(sender, sender_actor_id).map_err(Eip8130Error::invalid)?
        } else {
            Address::ZERO
        };
        Ok((policy_gated, policy_target))
    }

    /// Applies the transaction's account-configuration changes and installs the
    /// deferred account-*code* effects (created-account code and delegation)
    /// *without* authenticating the changes. Used by the read-only simulation so
    /// the post-change code the `calls` run against matches inclusion. The
    /// verifying pipeline instead routes through
    /// [`TransactionAuthorizer::authorize_and_apply`], which interleaves the same
    /// application with authorization against the evolving state.
    ///
    /// `now` is the block timestamp (Unix seconds) so the JIT expiry skip matches
    /// consensus: a lapsed unsequenced grant is dropped in both the estimate and at
    /// inclusion.
    pub fn apply_account_changes(
        signed: &Eip8130Signed,
        sctx: StorageCtx<'_>,
        sender: Address,
        now: u64,
    ) -> Result<(), Eip8130Error> {
        let mut acc_mut = AccountConfigurationStorage::new(sctx);
        let mut created_effect: Option<(Address, Bytes)> = None;
        let mut delegation_effect: Option<DelegationEffect> = None;
        for (index, change) in signed.tx().account_changes.iter().enumerate() {
            match change {
                AccountChange::Create(entry) => {
                    if delegation_effect.is_some() {
                        return Err(Eip8130Error::invalid(ApplyError::CreateAndDelegation));
                    }
                    if index != 0 || created_effect.is_some() {
                        return Err(Eip8130Error::invalid(ApplyError::InvalidCreatePosition));
                    }
                    let created = AccountChangeApplier::apply_create(&mut acc_mut, entry)
                        .map_err(Eip8130Error::invalid)?;
                    created_effect = Some((created.address, created.code));
                }
                AccountChange::ConfigChange(cc) => {
                    AccountChangeApplier::apply_config_change(
                        &mut acc_mut,
                        sender,
                        &cc.changes,
                        cc.channel,
                        cc.sequence,
                        now,
                    )
                    .map_err(Eip8130Error::invalid)?;
                }
                AccountChange::Delegation(Delegation { target }) => {
                    if delegation_effect.is_some() {
                        return Err(Eip8130Error::invalid(ApplyError::MultipleDelegations));
                    }
                    if created_effect.is_some() {
                        return Err(Eip8130Error::invalid(ApplyError::CreateAndDelegation));
                    }
                    delegation_effect = Some(DelegationEffect::new(sender, *target));
                }
            }
        }
        if let Some((address, code)) = &created_effect {
            Self::install_created_code(sctx, *address, code)?;
        }
        if let Some(delegation) = delegation_effect {
            delegation.install(sctx).map_err(Eip8130Error::invalid)?;
        }
        Ok(())
    }

    /// Installs a created account's runtime code, enforcing the CREATE2 collision
    /// rule the reference contract gets for free from a real deploy: the
    /// destination must be empty (no code, zero nonce). The account info is read
    /// from the real journal (not the config overlay), so block inclusion
    /// enforces what mempool admission checks separately — an inclusion path that
    /// bypasses the pool cannot overwrite preexisting third-party code.
    ///
    /// The runtime is already validated non-empty, `<= MAX_CODE_SIZE`, and not
    /// `0xEF`-prefixed by [`AccountChangeApplier::apply_create`], so it is always
    /// legacy bytecode.
    pub fn install_created_code(
        sctx: StorageCtx<'_>,
        address: Address,
        code: &Bytes,
    ) -> Result<(), Eip8130Error> {
        let occupied = sctx
            .with_account_info(address, |info| Ok(!info.is_empty_code_hash() || info.nonce != 0))
            .map_err(Eip8130Error::invalid)?;
        if occupied {
            return Err(Eip8130Error::invalid(
                "create destination already has code or a non-zero nonce",
            ));
        }
        sctx.set_code(address, Bytecode::new_legacy(code.clone())).map_err(Eip8130Error::invalid)
    }

    /// Computes the EIP-8130 intrinsic gas and the gas left for `calls`.
    /// Shared by both pipelines so intrinsic pricing is computed identically for
    /// execution and estimation.
    ///
    /// Errors when `gas_limit` is below either the EIP-7623 sender floor or
    /// sender-intrinsic gas. Both are checked: the floor dominates intrinsic gas
    /// only while the floor rate is at least the standard data rate, and a
    /// repriced schedule must be rejected rather than panic.
    pub fn resolve_execution_gas(
        signed: &Eip8130Signed,
        encoded: &[u8],
        input: &IntrinsicGasInput,
        gas_limit: u64,
    ) -> Result<(IntrinsicGas, u64), Eip8130Error> {
        let intrinsic =
            IntrinsicGas::compute(signed, encoded, input).map_err(Eip8130Error::invalid)?;
        if gas_limit < intrinsic.sender_floor() {
            return Err(Eip8130Error::invalid(
                "EIP-8130 gas limit is below the EIP-7623 calldata floor",
            ));
        }
        let execution_gas_available = intrinsic
            .execution_gas_available(gas_limit)
            .ok_or_else(|| Eip8130Error::invalid("EIP-8130 gas limit is below intrinsic gas"))?;
        Ok((intrinsic, execution_gas_available))
    }

    /// Folds the signed transaction-level refund counter into the final applied
    /// refund: clamps a net-negative counter to zero (a net negative grants no
    /// refund, never adds to gas owed), then applies EIP-3529's `gross_used / 5`
    /// ceiling. Accounting refunds signed across the whole transaction — rather
    /// than flooring each call's refund at zero — is what makes offsetting SSTORE
    /// refunds across calls cancel exactly, as under one continuous execution.
    pub fn capped_refund(signed_refund: i64, gross_used: u64) -> u64 {
        u64::try_from(signed_refund.max(0)).unwrap_or(0).min(gross_used / MAX_REFUND_QUOTIENT)
    }

    /// ABI-encodes the `ActorPolicyViolation(bytes32 actorId, address target)`
    /// protocol revert: the 4-byte selector followed by the two 32-byte words.
    pub fn actor_policy_violation_data(actor_id: B256, target: Address) -> Bytes {
        // `keccak256(b"ActorPolicyViolation(bytes32,address)")[..4]`, hardcoded to
        // avoid hashing on every policy-gate revert; pinned by a test.
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
    use alloy_sol_types::{SolError, sol};

    use super::*;

    fn outcome() -> Eip8130Outcome {
        Eip8130Outcome {
            sender: Address::ZERO,
            payer: Address::ZERO,
            sender_actor_id: B256::ZERO,
            policy_gated: false,
            policy_target: Address::ZERO,
            gas_limit: 1_000_000,
            sender_intrinsic: 100_000,
            sender_floor: 130_000,
            payer_auth: 7_000,
            payer_auth_standard: 5_000,
            execution_gas_available: 900_000,
            effective: 1_000,
            base_fee: 600,
            bump_protocol_nonce: false,
        }
    }

    #[test]
    fn billable_gas_applies_the_eip7623_sender_floor() {
        let outcome = outcome();
        // Cheap execution stays below the floor, so the floor (plus payer floor) is billed.
        assert_eq!(outcome.billable_gas(10_000, 0), outcome.sender_floor + outcome.payer_auth);
        // Heavy execution clears the floor, so the standard branch is billed.
        assert_eq!(
            outcome.billable_gas(500_000, 0),
            outcome.sender_intrinsic + 500_000 + outcome.payer_auth_standard,
        );
    }

    #[test]
    fn billable_gas_caps_the_refund_at_a_fifth_of_gross_sender_gas() {
        let outcome = outcome();
        let gross = outcome.sender_intrinsic + 500_000;
        assert_eq!(
            outcome.billable_gas(500_000, i64::MAX),
            gross - gross / MAX_REFUND_QUOTIENT + outcome.payer_auth_standard,
        );
        // A net-negative refund grants nothing and never adds gas.
        assert_eq!(outcome.billable_gas(500_000, -1_000), gross + outcome.payer_auth_standard);
    }

    #[test]
    fn capped_refund_uses_signed_transaction_level_accounting() {
        // Call A clears a slot (+4800), call B re-dirties it back to the original
        // (-1900 net): the signed sum cancels down to the reset refund, where a
        // per-call floor at zero would over-refund 4800.
        assert_eq!(Eip8130Pipeline::capped_refund(4800 - 1900, 1_000_000), 2900);
        // A net-negative counter grants no refund (never adds to gas owed).
        assert_eq!(Eip8130Pipeline::capped_refund(-1900, 1_000_000), 0);
        // EIP-3529's `gross_used / 5` ceiling still binds the clamped refund.
        assert_eq!(Eip8130Pipeline::capped_refund(10_000, 20_000), 4_000);
    }

    #[test]
    fn fee_split_routes_base_fee_and_tip() {
        let split = Eip8130FeeSplit::new(&outcome(), 10).unwrap();
        assert_eq!(split.fee, U256::from(10_000));
        assert_eq!(split.base_fee_amount, U256::from(6_000));
        assert_eq!(split.priority_amount, U256::from(4_000));
    }

    #[test]
    fn validity_window_is_inclusive_on_both_bounds() {
        let tx =
            TxEip8130 { valid_after: 2_000_000, valid_before: 3_000_000, ..Default::default() };
        let after = tx.valid_after_ms();
        let before = tx.valid_before_ms();
        assert!(Eip8130Pipeline::check_validity_window(&tx, after - 1).is_err());
        assert!(Eip8130Pipeline::check_validity_window(&tx, after).is_ok());
        assert!(Eip8130Pipeline::check_validity_window(&tx, before).is_ok());
        assert!(Eip8130Pipeline::check_validity_window(&tx, before + 1).is_err());
    }

    #[test]
    fn actor_policy_violation_data_is_abi_encoded() {
        sol! { error ActorPolicyViolation(bytes32 actorId, address target); }
        let actor_id = B256::repeat_byte(0x42);
        let target = Address::repeat_byte(0x24);
        assert_eq!(
            Eip8130Pipeline::actor_policy_violation_data(actor_id, target).as_ref(),
            ActorPolicyViolation { actorId: actor_id, target }.abi_encode().as_slice(),
        );
    }
}
