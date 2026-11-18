//! Engine-neutral EIP-8130 execution pipeline.
//!
//! The storage-backed pre-call pipeline (authorization, delegation, nonce, intrinsic gas,
//! fee checks, and the transaction-context publish), its read-only simulation variant, and the
//! gas-settlement arithmetic. Every step runs over a [`StorageCtx`], so the revm and EVM2 engines
//! execute the identical code against their own gas-free journal providers.

use core::fmt::Display;

use alloy_primitives::{Address, B256, U256, address};
use base_common_consensus::{Eip8130Constants, Eip8130Signed, TxEip8130};
use base_common_precompiles::{NonceManagerStorage, TxContextStorage};
use base_precompile_storage::StorageCtx;

use crate::{
    FeeCheck, FeeError, IntrinsicGas, NonceMode, NonceValidator, ResolvedSender,
    TransactionAuthorizer,
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
    /// The block timestamp in Unix milliseconds (the clock of the validity window and the
    /// nonce-free replay ring).
    pub now_ms: u64,
    /// The block base fee per gas.
    pub base_fee: u128,
    /// The EIP-7825 per-transaction gas cap.
    pub tx_gas_limit_cap: u64,
    /// Whether fees are charged. `eth_call`, `eth_estimateGas` and unvalidated
    /// `eth_simulateV1` disable fee charging: nothing is pre-charged or settled, so the payer's
    /// ability to afford the worst-case fee is not checked either.
    pub fee_charge: bool,
}

/// The resolved pre-call context of an EIP-8130 transaction: the authorized
/// sender and payer, and the gas/fee parameters needed to dispatch `calls` and
/// settle the fee.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Eip8130Outcome {
    /// The resolved transaction sender (the account dispatching `calls`).
    pub sender: Address,
    /// The resolved gas payer (the sender, for self-pay).
    pub payer: Address,
    /// The transaction's `gas_limit` (the sender-signed budget for sender
    /// authentication, intrinsic costs, account changes, and call execution).
    pub gas_limit: u64,
    /// Sender-intrinsic gas (intrinsic gas excluding payer authentication).
    pub sender_intrinsic: u64,
    /// EIP-7623 sender-side calldata floor: the minimum gas the transaction is
    /// metered against `gas_limit` regardless of how little its `calls` execute.
    /// `gas_limit` is guaranteed `>= sender_floor` (the transaction is rejected
    /// otherwise).
    pub sender_floor: u64,
    /// Payer-authentication gas charged to the payer on top of `gas_limit`,
    /// also used for the pre-charge and the EIP-7825 cap.
    pub payer_auth_cost: u64,
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
    /// included. Payer authentication already prices its bytes at the floor
    /// rate, so only the sender portion is floored:
    ///
    /// ```text
    /// max(sender_gas, sender_floor) + payer_auth_cost
    /// ```
    #[must_use]
    pub const fn metered_gas(&self, sender_gas: u64) -> u64 {
        self.floored_sender_gas(sender_gas).saturating_add(self.payer_auth_cost)
    }

    /// Worst-case chargeable gas: the full sender budget plus payer authentication, both billed
    /// at the effective price. This is what the payer is pre-charged for.
    #[must_use]
    pub const fn max_chargeable_gas(&self) -> u64 {
        FeeCheck::max_chargeable_gas(self.gas_limit, self.payer_auth_cost)
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
    /// The EIP-7623 floor applies to the sender portion ([`Self::metered_gas`]):
    /// a data-heavy transaction whose `calls` execute cheaply still pays
    /// `sender_floor + payer_auth_cost`. Mirrors revm's `eip7623_check_gas_floor`,
    /// which compares the floor against the post-refund gas used.
    ///
    /// Under gas-limit billing (`gas_limit_billing`, from `BaseFeature::GasLimitBilling`) the
    /// charge is the reserved budget instead, [`Self::max_chargeable_gas`]: exactly what prepay
    /// pre-charged and what the block executor and the builder reserve, regardless of what the
    /// `calls` consumed or refunded.
    #[must_use]
    pub fn billable_gas(&self, call_gas_spent: u64, refund: i64, gas_limit_billing: bool) -> u64 {
        if gas_limit_billing {
            return self.max_chargeable_gas();
        }
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

    /// Consensus-level validity window on the millisecond axis (`now_ms`, the
    /// block's millisecond timestamp). Validity bounds are normalized to Unix
    /// milliseconds (seconds bounds are scaled by 1000 per EIP-8130 Timestamp
    /// Normalization); `0` disables the respective bound. The upper bound is the
    /// shared expiry rule (`TxEip8130::is_expired_at`): inclusive for
    /// nonce-bearing transactions, exclusive for nonce-free ones, whose replay
    /// ring only records a strictly-future `valid_before`.
    pub fn check_validity_window(tx: &TxEip8130, now_ms: u64) -> Result<(), Eip8130Error> {
        let valid_after = tx.valid_after_ms();
        if valid_after != 0 && now_ms < valid_after {
            return Err(Eip8130Error::invalid("transaction is not yet valid"));
        }
        if tx.is_expired_at(now_ms) {
            return Err(Eip8130Error::invalid("transaction validity window has expired"));
        }
        Ok(())
    }

    /// Runs the storage-backed pre-call pipeline (nonce check, authorization,
    /// delegation, nonce advance, intrinsic gas, fee-cap check) and publishes the transaction
    /// context, returning the resolved [`Eip8130Outcome`]. Storage writes land on
    /// `sctx` directly; the caller MUST discard the transaction on error.
    ///
    /// `sctx` must be a gas-free journal view: the EIP-8130 schedule is the single
    /// source of gas accounting for the pre-call work. The validity window is checked
    /// separately by [`Self::check_validity_window`].
    pub fn authorize_and_apply(
        sctx: StorageCtx<'_>,
        signed: &Eip8130Signed,
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

        // Ordering note: the delegation (3) writes journal state before the nonce
        // advances (4). Any `Err` returned here makes the caller discard the
        // transaction, so these earlier writes never persist for a rejected
        // transaction.

        // 1. Validate the nonce against the pre-transaction state, before any
        //    authentication or delegation work, so a stale transaction fails on a
        //    single read.
        let resolved_sender = ResolvedSender::resolve(signed).map_err(Eip8130Error::invalid)?;
        let sender = resolved_sender.account();
        let protocol_nonce =
            sctx.with_account_info(sender, |info| Ok(info.nonce)).map_err(Eip8130Error::invalid)?;
        NonceValidator::validate(
            tx,
            sender,
            protocol_nonce,
            &NonceManagerStorage::new(sctx),
            NonceMode::Inclusion,
            block.now_ms,
        )
        .map_err(Eip8130Error::invalid)?;

        // 2. Authenticate the sender and payer by secp256k1 recovery. This reads no
        //    state.
        let authorized = TransactionAuthorizer::authorize(signed, resolved_sender)
            .map_err(Eip8130Error::invalid)?;
        let payer = authorized.payer;

        // 3. Install the delegation indicator.
        if let Some(delegation) = &authorized.delegation {
            delegation.install(sctx).map_err(Eip8130Error::invalid)?;
        }

        // 4. Advance the nonce validated in step 1. The protocol nonce (key 0) is
        //    bumped with the caller.
        let mut nonce_mgr = NonceManagerStorage::new(sctx);
        let bump_protocol_nonce = if nonce_key == Eip8130Constants::NONCE_KEY_MAX {
            nonce_mgr
                .check_and_mark_expiring_nonce(tx.replay_id(sender), valid_before, block.now_ms)
                .map_err(Eip8130Error::invalid)?;
            false
        } else if nonce_key == U256::ZERO {
            true
        } else {
            nonce_mgr.increment_nonce(sender, nonce_key).map_err(Eip8130Error::invalid)?;
            false
        };

        // 5. Intrinsic gas under the EIP-8130 schedule.
        let (intrinsic, execution_gas_available) =
            Self::resolve_execution_gas(signed, sender, gas_limit)?;
        // EIP-7825: the transaction may consume `gas_limit` plus the payer
        // authentication metered on top of it, and that total is bounded by the
        // per-transaction gas cap (from Azul).
        if FeeCheck::max_chargeable_gas(gas_limit, intrinsic.payer_auth_cost)
            > block.tx_gas_limit_cap
        {
            return Err(Eip8130Error::invalid(
                "transaction gas exceeds the EIP-7825 per-transaction cap",
            ));
        }

        // 6. Fee caps and, when fees will be charged, the payer's balance. A fee-free execution
        //    reads nothing from the payer here, matching `simulate_resolve`, so the payer's
        //    warmth is the same on both paths.
        FeeCheck::validate_fees(max_fee, max_priority, block.base_fee)
            .map_err(Eip8130Error::invalid)?;
        if block.fee_charge {
            let payer_balance = sctx
                .with_account_info(payer, |info| Ok(info.balance))
                .map_err(Eip8130Error::invalid)?;
            FeeCheck::validate_balance(
                payer_balance,
                gas_limit,
                intrinsic.payer_auth_cost,
                max_fee,
            )
            .map_err(|error| match error {
                FeeError::InsufficientBalance { balance, required } => {
                    Eip8130Error::PayerCannotPay { balance, required }
                }
                error => Eip8130Error::invalid(error),
            })?;
        }

        // 7. Publish the transaction context (sender / payer / actor id) so it is
        //    readable by the `TxContext` precompile during `calls`.
        TxContextStorage::new(sctx)
            .set_context(sender, payer, Self::sender_actor_id(sender))
            .map_err(Eip8130Error::invalid)?;

        Ok(Eip8130Outcome {
            sender,
            payer,
            gas_limit,
            sender_intrinsic: intrinsic.sender_intrinsic(),
            sender_floor: intrinsic.sender_floor(),
            payer_auth_cost: intrinsic.payer_auth_cost,
            execution_gas_available,
            effective: FeeCheck::effective_gas_price(max_fee, max_priority, block.base_fee),
            base_fee: block.base_fee,
            bump_protocol_nonce,
        })
    }

    /// Resolves the [`Eip8130Outcome`] for read-only simulation (`eth_call` /
    /// `eth_estimateGas`): checks the auth shape, prices intrinsic gas and checks
    /// it against the gas limit, then installs the delegation — no signature
    /// recovery — and advances the nonce without validating it, so calls that
    /// read the Nonce Manager see the sequence they would on inclusion. The payer
    /// balance is not checked. Storage writes land on `sctx`; the caller reverts
    /// them.
    pub fn simulate_resolve(
        sctx: StorageCtx<'_>,
        signed: &Eip8130Signed,
        sender: Address,
        block: &Eip8130BlockContext,
    ) -> Result<Eip8130Outcome, Eip8130Error> {
        let tx = signed.tx();
        // EIP-7825: execution bounds `gas_limit` plus payer authentication by the
        // per-transaction cap, so simulate at most the gas a real transaction could
        // carry.
        let payer_auth_cost = IntrinsicGas::payer_auth_cost_of(signed);
        let gas_limit = tx.gas_limit.min(block.tx_gas_limit_cap.saturating_sub(payer_auth_cost));
        let max_fee = tx.max_fee_per_gas;
        let max_priority = tx.max_priority_fee_per_gas;
        // Use the declared payer (sponsor) so the payer published to the
        // `TxContext` precompile matches a real execution. A named payer's
        // signature is not verified here. An open payer is the signer its
        // `payer_auth` recovers to; before a payer has signed the payer is unknown
        // and published as `UNSIGNED_OPEN_PAYER`, never the sender.
        let payer = signed.resolved_payer(sender).unwrap_or(Self::UNSIGNED_OPEN_PAYER);
        TransactionAuthorizer::check(signed).map_err(Eip8130Error::invalid)?;

        // 1. Intrinsic gas. It is a pure function of the signed transaction and its
        //    sender, so it equals what execution charges. Estimation does not
        //    validate the nonce. A `gas_limit` that cannot cover it is rejected
        //    before the delegation is installed.
        let (intrinsic, execution_gas_available) =
            Self::resolve_execution_gas(signed, sender, gas_limit)?;

        // 2. Install the delegation so the calls run against post-change code.
        if let Some(delegation) =
            TransactionAuthorizer::delegation(signed, sender).map_err(Eip8130Error::invalid)?
        {
            delegation.install(sctx).map_err(Eip8130Error::invalid)?;
        }

        // 3. Advance a 2D nonce channel after the delegation, as inclusion does.
        //    The protocol nonce is bumped with the caller overwrite in the engine's
        //    simulate; nonce-free replay ids are not readable from the EVM, so they
        //    are not recorded.
        if !tx.nonce_key.is_zero() && tx.nonce_key != Eip8130Constants::NONCE_KEY_MAX {
            NonceManagerStorage::new(sctx)
                .increment_nonce(sender, tx.nonce_key)
                .map_err(Eip8130Error::invalid)?;
        }

        // 4. Publish the transaction context for the `TxContext` precompile.
        TxContextStorage::new(sctx)
            .set_context(sender, payer, Self::sender_actor_id(sender))
            .map_err(Eip8130Error::invalid)?;

        Ok(Eip8130Outcome {
            sender,
            payer,
            gas_limit,
            sender_intrinsic: intrinsic.sender_intrinsic(),
            sender_floor: intrinsic.sender_floor(),
            payer_auth_cost: intrinsic.payer_auth_cost,
            execution_gas_available,
            effective: FeeCheck::effective_gas_price(max_fee, max_priority, block.base_fee),
            base_fee: block.base_fee,
            bump_protocol_nonce: tx.nonce_key.is_zero(),
        })
    }

    /// Computes the EIP-8130 intrinsic gas and the gas left for `calls`.
    /// Shared by both pipelines so intrinsic pricing is computed identically for
    /// execution and estimation.
    ///
    /// Errors when `gas_limit` is below either the EIP-7623 sender floor or
    /// sender-intrinsic gas. Both are checked because neither dominates: the
    /// floor excludes the state-dependent costs but meters data at the higher
    /// rate.
    pub fn resolve_execution_gas(
        signed: &Eip8130Signed,
        sender: Address,
        gas_limit: u64,
    ) -> Result<(IntrinsicGas, u64), Eip8130Error> {
        let intrinsic = IntrinsicGas::compute(signed, sender);
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

    /// The sender actor id published to the `TxContext` precompile: the sender
    /// address, right-aligned in a 32-byte word.
    pub fn sender_actor_id(sender: Address) -> B256 {
        sender.into_word()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome() -> Eip8130Outcome {
        Eip8130Outcome {
            sender: Address::ZERO,
            payer: Address::ZERO,
            gas_limit: 1_000_000,
            sender_intrinsic: 100_000,
            sender_floor: 130_000,
            payer_auth_cost: 5_000,
            execution_gas_available: 900_000,
            effective: 1_000,
            base_fee: 600,
            bump_protocol_nonce: false,
        }
    }

    #[test]
    fn billable_gas_applies_the_eip7623_sender_floor() {
        let outcome = outcome();
        // Cheap execution stays below the floor, so the floor (plus payer auth) is billed.
        assert_eq!(
            outcome.billable_gas(10_000, 0, false),
            outcome.sender_floor + outcome.payer_auth_cost
        );
        // Heavy execution clears the floor, so the standard branch is billed.
        assert_eq!(
            outcome.billable_gas(500_000, 0, false),
            outcome.sender_intrinsic + 500_000 + outcome.payer_auth_cost,
        );
    }

    #[test]
    fn gas_limit_billing_bills_the_reserved_budget() {
        let outcome = outcome();
        let reserved = outcome.gas_limit + outcome.payer_auth_cost;
        for (spent, refund) in [(10_000, 0), (500_000, 0), (500_000, 100_000)] {
            assert_eq!(outcome.billable_gas(spent, refund, true), reserved);
        }
    }

    #[test]
    fn billable_gas_caps_the_refund_at_a_fifth_of_gross_sender_gas() {
        let outcome = outcome();
        let gross = outcome.sender_intrinsic + 500_000;
        assert_eq!(
            outcome.billable_gas(500_000, i64::MAX, false),
            gross - gross / MAX_REFUND_QUOTIENT + outcome.payer_auth_cost,
        );
        // A net-negative refund grants nothing and never adds gas.
        assert_eq!(outcome.billable_gas(500_000, -1_000, false), gross + outcome.payer_auth_cost);
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
}
