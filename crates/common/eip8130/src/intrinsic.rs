//! EIP-8130 intrinsic gas: the total cost to include an AA transaction.

use alloy_primitives::{Address, U256};
use base_common_consensus::{
    AccountChange, ChangeType, Eip8130Constants, Eip8130Contracts, Eip8130Signed, SignedChange,
};

use crate::Eip8130GasSchedule;

/// Reason intrinsic gas cannot be computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum IntrinsicGasError {
    /// A transaction authenticator has no execution-gas entry in the schedule.
    /// This is a configuration error rather than an attacker-reachable state:
    /// dispatch only admits canonical authenticators, every one of which the
    /// schedule prices. It fires if a new authenticator is added to the dispatch
    /// allowlist but not to the schedule — surfacing the omission here instead of
    /// silently undercharging the transaction. A nested delegate authenticator
    /// (depth-2 delegation, which dispatch rejects) also lands here, since the
    /// delegate authenticator is not a priced *leaf*.
    #[error("no gas-schedule entry for authenticator {0}")]
    UnscheduledAuthenticator(Address),
    /// `payer_auth_cost` exceeds [`Eip8130GasSchedule::MAX_AUTHENTICATION_GAS`].
    #[error("payer authentication gas {0} exceeds MAX_AUTHENTICATION_GAS")]
    PayerAuthGasExceeded(u64),
}

/// Wire encoding of an authentication blob, selecting how it is parsed and
/// priced. This is the encoding shape, not the account type: an implicit-EOA
/// owner is [`Self::BareSignature`] on the `sender_auth` path but
/// [`Self::Prefixed`] when it names itself as `K1_AUTHENTICATOR || sig` inside a
/// config change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthWireForm {
    /// A raw 65-byte secp256k1 signature with no authenticator prefix: the
    /// empty-`sender` (default-EOA) path, priced as a k1 authentication over a
    /// single account-state SLOAD.
    BareSignature,
    /// An `authenticator(20) || data` blob: every other surface — a configured
    /// sender, any payer, and every `cfg.auth`.
    Prefixed,
}

impl AuthWireForm {
    /// The wire form of a transaction's `sender_auth`: a bare signature on the
    /// empty-`sender` (EOA) path, otherwise an `authenticator || data` blob.
    #[must_use]
    pub const fn for_sender(sender: Option<Address>) -> Self {
        match sender {
            Some(_) => Self::Prefixed,
            None => Self::BareSignature,
        }
    }
}

/// State-derived inputs the transaction body alone cannot determine.
///
/// These flags come from the caller's state view (the nonce manager, account
/// configuration, and sender code), supplied so this crate stays a pure function
/// of the transaction plus these hints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct IntrinsicGasInput {
    /// The resolved sender. Calls back to the sender carry no
    /// `value_transfer_cost`.
    pub sender: Address,
    /// Whether this transaction's sequence nonce channel is being used for the
    /// first time (its current nonce is zero) — selects the SSTORE *set* cost
    /// over the *reset* cost. Ignored for nonce-free (`NONCE_KEY_MAX`)
    /// transactions.
    pub nonce_key_first_use: bool,
    /// Number of a transaction's revoke slots that execution resolved to be empty
    /// zero-to-zero touches, which [`Eip8130GasSchedule::ACTOR_REVOKE_COST`] priced
    /// conservatively as `SSTORE` resets. Each such slot discounts the charge by
    /// the reset-vs-cold-noop delta ([`Eip8130GasSchedule::COLD_SLOT_RESET_DISCOUNT`]).
    ///
    /// A revoke of the account's inline secp256k1 self key contributes one slot
    /// per actually-empty slot: its `actor_config` is always empty, plus each
    /// policy slot (`manager`, `commitment`) whose stored value is zero — 3 when
    /// ungated (both policy slots unwritten) and 1 to 3 when policy-gated, since a
    /// gated actor may still carry a zero manager and/or commitment. The intrinsic
    /// computation bounds this hint by three slots per charged revoke, preventing a
    /// mismatched caller from discounting uncharged slots. A zero (unresolved)
    /// count leaves the conservative reset price, so this can only reduce, never
    /// under-price, the charge.
    pub revoke_discount_slots: u32,
}

impl IntrinsicGasInput {
    /// Creates the intrinsic-gas state hints.
    #[must_use]
    pub const fn new(sender: Address, nonce_key_first_use: bool) -> Self {
        Self { sender, nonce_key_first_use, revoke_discount_slots: 0 }
    }

    /// Adds the count of empty zero-to-zero revoke slots resolved during
    /// account-change application, used to discount their over-conservative
    /// reset price. See [`Self::revoke_discount_slots`].
    #[must_use]
    pub const fn with_revoke_discount_slots(mut self, revoke_discount_slots: u32) -> Self {
        self.revoke_discount_slots = revoke_discount_slots;
        self
    }

    /// Safe-ceiling input shared by the estimation (`eth_estimateGas` /
    /// `eth_call`) and mempool-admission paths.
    ///
    /// It pins the non-monotonic, state-dependent cost to its worst case — zero
    /// revoke discount (every revoke slot priced as a full reset) — so a
    /// `gas_limit` sized from the estimate can never be rejected at admission nor
    /// OOG at inclusion. Execution ([`Self::with_revoke_discount_slots`] with the
    /// resolved value) reprices it precisely and can only meet or undercharge
    /// this ceiling.
    ///
    /// Defining the ceiling here keeps estimation and admission from silently
    /// drifting apart: both must feed [`IntrinsicGas::compute`] the *same* pinned
    /// input for the `estimate == admission >= execution` guarantee to hold.
    #[must_use]
    pub const fn worst_case(sender: Address, nonce_key_first_use: bool) -> Self {
        Self::new(sender, nonce_key_first_use).with_revoke_discount_slots(0)
    }
}

/// The EIP-8130 intrinsic-gas breakdown, one field per spec component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct IntrinsicGas {
    /// `AA_BASE_COST`.
    pub base: u64,
    /// `tx_payload_cost` — EIP-2028 data-availability cost over the encoding
    /// with an empty `payer_auth`.
    pub payload: u64,
    /// `nonce_key_cost`.
    pub nonce_key: u64,
    /// `value_transfer_cost` — [`Eip8130GasSchedule::TX_VALUE_COST`] per call
    /// with `value > 0` and `to != sender`.
    pub value_transfer: u64,
    /// `bytecode_cost` — account creation.
    pub bytecode: u64,
    /// `account_changes_cost` — config-change and delegation entries.
    pub account_changes: u64,
    /// `sender_auth_cost` — sender authenticator execution + its `authorize`
    /// SLOAD(s) (see `auth_sloads`).
    pub sender_auth: u64,
    /// `payer_auth_cost` — payer authenticator execution + its `authorize`
    /// SLOAD(s) + the data cost of the `payer_auth` bytes, or `0` for self-pay.
    pub payer_auth: u64,
}

impl IntrinsicGas {
    /// Total intrinsic gas (all components).
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.base
            .saturating_add(self.payload)
            .saturating_add(self.nonce_key)
            .saturating_add(self.value_transfer)
            .saturating_add(self.bytecode)
            .saturating_add(self.account_changes)
            .saturating_add(self.sender_auth)
            .saturating_add(self.payer_auth)
    }

    /// Sender-intrinsic gas: intrinsic gas excluding `payer_auth_cost`, which is
    /// the portion bounded by `gas_limit` (payer authentication is metered on
    /// top of `gas_limit`).
    #[must_use]
    pub const fn sender_intrinsic(&self) -> u64 {
        self.total().saturating_sub(self.payer_auth)
    }

    /// Gas available to `calls` after sender-intrinsic gas, or `None` when
    /// sender-intrinsic gas alone exceeds `gas_limit` (the transaction is
    /// underfunded and cannot be included).
    #[must_use]
    pub const fn execution_gas_available(&self, gas_limit: u64) -> Option<u64> {
        gas_limit.checked_sub(self.sender_intrinsic())
    }

    /// Computes the intrinsic gas for a signed EIP-8130 transaction.
    ///
    /// `encoded` is the EIP-2718-serialized signed transaction
    /// (`type_byte || rlp([..fields.., sender_auth, payer_auth])`) — the same
    /// bytes used for networking and the transaction hash. It is taken as a
    /// parameter, rather than re-serialized here, because `compute` runs for
    /// every transaction on both the mempool-admission and block-building paths,
    /// where the caller already holds the serialized form; it feeds only the
    /// EIP-2028 `payload` cost. A sponsored transaction is re-serialized with an
    /// empty `payer_auth`, since the payer's bytes are billed in `payer_auth`
    /// rather than against the sender's `gas_limit`.
    ///
    /// Returns [`IntrinsicGasError::UnscheduledAuthenticator`] if any sender,
    /// payer, or config-change authenticator lacks a gas-schedule entry, and
    /// [`IntrinsicGasError::PayerAuthGasExceeded`] if `payer_auth_cost` exceeds
    /// [`Eip8130GasSchedule::MAX_AUTHENTICATION_GAS`].
    #[must_use = "discarding the result silently skips the entire intrinsic-gas computation"]
    pub fn compute(
        signed: &Eip8130Signed,
        encoded: &[u8],
        input: &IntrinsicGasInput,
    ) -> Result<Self, IntrinsicGasError> {
        let tx = signed.tx();

        let nonce_key = if tx.nonce_key == Eip8130Constants::NONCE_KEY_MAX {
            Eip8130GasSchedule::NONCE_FREE_COST
        } else if input.nonce_key_first_use {
            Eip8130GasSchedule::NONCE_KEY_FIRST_USE_COST
        } else {
            Eip8130GasSchedule::NONCE_KEY_EXISTING_COST
        };

        let mut bytecode = 0u64;
        let mut account_changes = 0u64;
        let mut revoke_change_count = 0u32;
        // All account changes in a transaction target the same (`sender`) packed
        // account-state slot: a create bootstraps it and every config change bumps
        // a sequence in it. Only the first such access is a cold zero-to-nonzero
        // write; once warmed, later config-change bumps are warm SLOAD + reset.
        let mut account_state_touched = false;
        for change in &tx.account_changes {
            match change {
                AccountChange::Create(entry) => {
                    // `bytecode_cost`: deployment base + per-byte code deposit.
                    let deposit = Eip8130GasSchedule::CODE_DEPOSIT_PER_BYTE
                        .saturating_mul(u64::try_from(entry.code.len()).unwrap_or(u64::MAX));
                    bytecode = bytecode
                        .saturating_add(Eip8130GasSchedule::CREATE_BASE_COST)
                        .saturating_add(deposit);
                    // Bootstrap `account_state` (`local_sequence = 1` and the
                    // default-EOA-revoked flag) before installing initial actors.
                    // This is the first (cold) write to the packed slot, so a
                    // later config change on the same account only pays the warm
                    // reset.
                    account_changes =
                        account_changes.saturating_add(Eip8130GasSchedule::ACCOUNT_STATE_SET_COST);
                    account_state_touched = true;
                    // Each initial actor writes one fresh `actor_config` slot, plus
                    // the two policy slots (`policy_manager` + `policy_commitment`)
                    // when it attaches a 52-byte `policyData` — a policy initial
                    // actor is 3 slot-sets versus 1 for a non-policy actor.
                    // Attachment is length-based (base/eip-8130 #95), decoupled
                    // from `SCOPE_POLICY`. These slot writes are metered per actor,
                    // mirroring the `ConfigChange` per-slot accounting below —
                    // creation must not register actors for free relative to a
                    // later config change authorizing the same set.
                    for actor in &entry.initial_actors {
                        let mut cost = Eip8130GasSchedule::ACTOR_SLOT_SET_COST;
                        if !actor.policy_data.is_empty() {
                            cost = cost.saturating_add(
                                Eip8130GasSchedule::ACTOR_SLOT_SET_COST.saturating_mul(2),
                            );
                        } else {
                            cost = cost.saturating_add(Eip8130GasSchedule::POLICY_SLOTS_NOOP_COST);
                        }
                        account_changes = account_changes.saturating_add(cost);
                    }
                }
                AccountChange::ConfigChange(cc) => {
                    // One packed account-state fetch supplies lock status and both
                    // sequence channels; the final write advances the selected
                    // channel. The first access to this slot in the transaction is
                    // a cold zero-to-nonzero write; any earlier create or config
                    // change already warmed it, so a subsequent bump is only a warm
                    // SLOAD + reset.
                    let state_cost = if account_state_touched {
                        Eip8130GasSchedule::CONFIG_CHANGE_STATE_COST_SUBSEQUENT
                    } else {
                        account_state_touched = true;
                        Eip8130GasSchedule::CONFIG_CHANGE_STATE_COST
                    };
                    account_changes = account_changes.saturating_add(state_cost);
                    // `signature` is always `authenticator || data` (never a bare
                    // signature); an implicit-EOA owner names itself explicitly as
                    // `K1_AUTHENTICATOR || sig` here.
                    let auth = Self::auth_cost(cc.signature.as_ref(), AuthWireForm::Prefixed)?;
                    account_changes = account_changes.saturating_add(auth);
                    for op in &cc.changes {
                        if op.change_type == ChangeType::RevokeActor {
                            revoke_change_count = revoke_change_count.saturating_add(1);
                        }
                        account_changes =
                            account_changes.saturating_add(Self::actor_change_write_cost(op));
                    }
                }
                AccountChange::Delegation(_) => {
                    account_changes =
                        account_changes.saturating_add(Eip8130GasSchedule::DELEGATION_DEPOSIT_COST);
                }
            }
        }

        // `actor_change_write_cost` prices every revoke as three slot resets
        // (worst case). Some of those slots are actually empty zero-to-zero touches
        // (an inline secp256k1 self revoke's `actor_config` slot always, and its
        // policy slots when the self was ungated), so discount each empty slot that
        // execution resolved. Applied here rather than in the per-change loop
        // because whether a revoke's slots are empty is state-derived, not visible
        // from the transaction body.
        debug_assert!(
            input.revoke_discount_slots <= revoke_change_count.saturating_mul(3),
            "resolved revoke discount slots exceed the three-slot-per-revoke maximum"
        );
        // Keep release builds safe if a future caller misthreads the execution
        // hint: the discount can never cover more slots than the transaction body
        // was charged resets for (three per revoke).
        let discounted_slots =
            Self::bounded_revoke_discount_slots(input.revoke_discount_slots, revoke_change_count);
        let revoke_discount = Eip8130GasSchedule::COLD_SLOT_RESET_DISCOUNT
            .saturating_mul(u64::from(discounted_slots));
        account_changes = account_changes.saturating_sub(revoke_discount);

        // Only the empty-`sender` path (`sender == None`) is a bare 65-byte
        // signature parsed via native ecrecover; a configured sender (and every
        // payer) is an `authenticator || data` blob and must not be parsed as a
        // bare signature.
        let sender_auth =
            Self::auth_cost(signed.sender_auth().as_ref(), AuthWireForm::for_sender(tx.sender))?;
        let payer_auth = Self::max_payer_auth_cost(signed)?;

        let payload = if signed.payer_auth().is_empty() {
            Self::data_cost(encoded)
        } else {
            Self::data_cost(&signed.encoded_2718_without_payer_auth())
        };

        let value_calls = tx
            .calls
            .iter()
            .flatten()
            .filter(|call| !call.value.is_zero() && call.to != input.sender)
            .count();
        let value_transfer = Eip8130GasSchedule::TX_VALUE_COST
            .saturating_mul(u64::try_from(value_calls).unwrap_or(u64::MAX));

        Ok(Self {
            base: Eip8130GasSchedule::AA_BASE_COST,
            payload,
            nonce_key,
            value_transfer,
            bytecode,
            account_changes,
            sender_auth,
            payer_auth,
        })
    }

    /// Bounds an execution-resolved empty-slot discount count by the maximum the
    /// intrinsic accounting actually charged resets for: three slots per revoke.
    const fn bounded_revoke_discount_slots(reported: u32, revoke_change_count: u32) -> u32 {
        let max = revoke_change_count.saturating_mul(3);
        if reported < max { reported } else { max }
    }

    /// Payer-authentication gas billed *on top of* `gas_limit` for a signed
    /// EIP-8130 transaction (`0` for self-pay).
    ///
    /// Block gas reservation uses this to budget the payer's authentication in
    /// addition to the sender-signed `gas_limit`: the payer reimburses its own
    /// authentication beyond that limit, so a block admitting a transaction on
    /// `gas_limit` alone could let true consumption push cumulative gas over the
    /// block limit. The charge is fully determined by the auth blob (authenticator
    /// execution gas, its cold `authorize` SLOAD, and the data cost of its
    /// bytes), so building and validation share this exact bound.
    #[must_use = "discarding the result skips the payer-authentication reservation"]
    pub fn max_payer_auth_cost(signed: &Eip8130Signed) -> Result<u64, IntrinsicGasError> {
        if signed.tx().payer.is_none() {
            return Ok(0);
        }
        let payer_auth = signed.payer_auth().as_ref();
        let cost = Self::auth_cost(payer_auth, AuthWireForm::Prefixed)?
            .saturating_add(Self::data_cost(payer_auth));
        if cost > Eip8130GasSchedule::MAX_AUTHENTICATION_GAS {
            return Err(IntrinsicGasError::PayerAuthGasExceeded(cost));
        }
        Ok(cost)
    }

    /// EIP-2028 data cost of `bytes`.
    fn data_cost(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0u64, |acc, &byte| {
            let cost = if byte == 0 {
                Eip8130GasSchedule::TX_DATA_ZERO_BYTE
            } else {
                Eip8130GasSchedule::TX_DATA_NONZERO_BYTE
            };
            acc.saturating_add(cost)
        })
    }

    /// Cost of authenticating one auth blob: authenticator execution gas plus the
    /// cold SLOADs the `authorize` step reads.
    ///
    /// `form` selects how the blob is parsed:
    /// [`AuthWireForm::BareSignature`] is a raw 65-byte secp256k1 signature with
    /// no authenticator prefix (the empty-`sender` path, charged k1);
    /// [`AuthWireForm::Prefixed`] is an `authenticator(20) || data` blob (every
    /// other surface, including the implicit-EOA owner naming itself as
    /// `K1_AUTHENTICATOR || sig`).
    ///
    /// See [`Self::auth_sloads`] for how the SLOAD count is derived.
    fn auth_cost(auth: &[u8], form: AuthWireForm) -> Result<u64, IntrinsicGasError> {
        let exec = Self::auth_exec_cost(auth, form)?;
        let sloads = Self::auth_sloads(auth, form, exec);
        Ok(exec.saturating_add(Eip8130GasSchedule::COLD_SLOAD.saturating_mul(sloads)))
    }

    /// Number of cold SLOADs the `authorize` step reads for one authentication.
    ///
    /// - **Bare signature** (default-EOA wire form): one account-state SLOAD that
    ///   carries the inline self config (scope/policy/expiry and the
    ///   `DEFAULT_EOA_REVOKED` flag), resolving the self key in a single read.
    /// - **Any resolved authenticator** (explicit `K1_AUTHENTICATOR`, P-256,
    ///   `WebAuthn`, delegate): one cold SLOAD. The inline self-config model
    ///   collapses the former permissioned-self worst case (account-state *and*
    ///   `actor_config`) to a single read, so an explicit k1 self and a non-self
    ///   k1 actor each read exactly one slot.
    /// - **A degenerate sub-20-byte prefixed blob** resolves no authenticator and
    ///   reads no slot, so it costs `0` rather than a phantom SLOAD. Such blobs
    ///   are unreachable here (dispatch rejects them upstream); guarding keeps the
    ///   SLOAD tied to a real read.
    fn auth_sloads(auth: &[u8], form: AuthWireForm, exec: u64) -> u64 {
        if matches!(form, AuthWireForm::BareSignature) {
            return 1;
        }
        match Self::authenticator_of(auth) {
            Some(_) if exec > 0 => 1,
            _ => 0,
        }
    }

    /// Authenticator *execution* gas for an auth blob, resolving the delegate
    /// authenticator's nested authenticator at depth-1. See [`Self::auth_cost`]
    /// for the meaning of `form`.
    fn auth_exec_cost(auth: &[u8], form: AuthWireForm) -> Result<u64, IntrinsicGasError> {
        if matches!(form, AuthWireForm::BareSignature) {
            return Ok(Eip8130GasSchedule::AUTH_EXEC_K1);
        }
        let Some(authenticator) = Self::authenticator_of(auth) else {
            return Ok(0);
        };
        // A configured k1 actor (including a re-registered self key, or an
        // implicit-EOA owner authorizing a config change) is named explicitly as
        // `K1_AUTHENTICATOR || sig` and priced via `leaf_exec_gas` below.
        // `address(0)` is the empty "no actor configured" sentinel and is never a
        // valid authenticator selector, so it falls through to
        // `UnscheduledAuthenticator` here just as dispatch rejects it upstream.
        if authenticator == Eip8130Contracts::DELEGATE_AUTHENTICATOR {
            // blob = delegate_authenticator(20) || delegate_account(20) ||
            // nested_authenticator(20) || nested_data; the nested blob
            // (authenticator || data) starts after both 20-byte prefixes. The
            // nested authenticator is resolved as a *leaf* (never via the
            // delegate branch), so depth-1 is enforced here rather than relying
            // on dispatch: a nested delegate is not a priced leaf and errors. A
            // 40..60-byte blob carries no nested authenticator, so it charges the
            // delegate overhead alone here plus the outer authorize SLOAD (via
            // `auth_sloads`) — a safe overcharge on a blob dispatch rejects before
            // this runs, not a reachable underprice.
            let nested_exec = match auth.get(40..).and_then(Self::authenticator_of) {
                Some(nested) => Self::leaf_exec_gas(nested)?,
                None => 0,
            };
            return Ok(Eip8130GasSchedule::AUTH_EXEC_DELEGATE_OVERHEAD.saturating_add(nested_exec));
        }
        Self::leaf_exec_gas(authenticator)
    }

    /// Execution gas for a leaf (non-delegate) enshrined authenticator, erroring
    /// when the address has no schedule entry.
    fn leaf_exec_gas(authenticator: Address) -> Result<u64, IntrinsicGasError> {
        Eip8130GasSchedule::leaf_auth_exec_gas(authenticator)
            .ok_or(IntrinsicGasError::UnscheduledAuthenticator(authenticator))
    }

    /// The authenticator address at the head of a configured-actor auth blob, or
    /// `None` when the blob is too short to carry one.
    fn authenticator_of(auth: &[u8]) -> Option<Address> {
        (auth.len() >= 20).then(|| Address::from_slice(&auth[..20]))
    }

    /// Storage-write cost for one actor change: an authorize sets the
    /// `actor_config` slot (plus the two policy slots when it carries a policy);
    /// a revoke clears the actor config and both policy slots.
    fn actor_change_write_cost(op: &SignedChange) -> u64 {
        match op.change_type {
            ChangeType::RevokeActor => Eip8130GasSchedule::ACTOR_REVOKE_COST,
            ChangeType::AuthorizeActor => {
                let mut cost = Eip8130GasSchedule::ACTOR_SLOT_SET_COST;
                if Self::authorize_attaches_policy(op.payload.as_ref()) {
                    // policy_commitment + policy_manager.
                    cost = cost
                        .saturating_add(Eip8130GasSchedule::ACTOR_SLOT_SET_COST.saturating_mul(2));
                } else {
                    cost = cost.saturating_add(Eip8130GasSchedule::POLICY_SLOTS_NOOP_COST);
                }
                cost
            }
            // IncrementLocalEpoch carries no per-actor slot writes; it rewrites
            // the packed account-state slot the config change's sequence advance
            // already touched, priced as a warm dirty SSTORE.
            ChangeType::IncrementLocalEpoch => Eip8130GasSchedule::INCREMENT_LOCAL_EPOCH_COST,
            // Lock / Unlock apply handlers are not yet enshrined; priced when
            // wired in.
            ChangeType::Lock | ChangeType::Unlock => 0,
        }
    }

    /// Whether an authorize op's ABI-encoded
    /// `(bytes32 actorId, ActorConfig, bytes policyData)` `payload` attaches a
    /// policy — i.e. carries a 52-byte `policyData`. Attachment is length-based
    /// (base/eip-8130 #95) and decoupled from `SCOPE_POLICY`, matching the slots
    /// execution actually writes.
    ///
    /// The params encoding is four static head words (`actorId`, `authenticator`,
    /// `expiry`, `scope`) then the `policyData` offset word, and the `policyData`
    /// `(length, data)` tail lives at that offset. This **follows the offset
    /// pointer** rather than assuming the canonical `0xA0`, so the length read
    /// here is exactly the one the revm apply path's `AccountChangeApplier::decode_authorize`
    /// reads (the `authorize_attaches_policy_agrees_with_apply_decode` test in
    /// `base-execution-eip8130` pins this): the metering can never disagree with
    /// execution on the danger direction
    /// (metering "no policy" while a 52-byte policy is written). A payload whose
    /// pointer is out of range is metered as no policy — the validating decoder
    /// rejects it and the transaction reverts regardless.
    ///
    /// Public so the revm apply path (`base-execution-eip8130`) can pin its
    /// validating decoder's policy detection against this metering predicate.
    pub fn authorize_attaches_policy(payload: &[u8]) -> bool {
        const OFFSET_WORD: usize = 4 * 32;
        if payload.len() < OFFSET_WORD + 32 {
            return false;
        }
        let Ok(offset) =
            usize::try_from(U256::from_be_slice(&payload[OFFSET_WORD..OFFSET_WORD + 32]))
        else {
            return false;
        };
        offset.checked_add(32).is_some_and(|length_end| length_end <= payload.len())
            && U256::from_be_slice(&payload[offset..offset + 32])
                == U256::from(Eip8130Constants::POLICY_DATA_LEN)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, Bytes, U256, address};
    use alloy_sol_types::SolValue;
    use base_common_consensus::{
        AccountChange, AccountChangeChannel, Call, ChangeType, CreateEntry, Delegation,
        InitialActor, SignedAccountChanges, SignedChange, TxEip8130,
    };

    use super::*;

    /// Builds an `AuthorizeActor` op payload `abi.encode(actorId, ActorConfig, policyData)`.
    fn authorize_op(
        actor_id: B256,
        authenticator: Address,
        scope: u16,
        expiry: u64,
    ) -> SignedChange {
        let abi =
            ActorConfigAbi { authenticator, scope, expiry: alloy_primitives::Uint::from(expiry) };
        SignedChange {
            change_type: ChangeType::AuthorizeActor,
            payload: Bytes::from((actor_id, abi, Bytes::new()).abi_encode_params()),
        }
    }

    /// Builds a `RevokeActor` op payload `abi.encode(actorId)`.
    fn revoke_op(actor_id: B256) -> SignedChange {
        SignedChange {
            change_type: ChangeType::RevokeActor,
            payload: Bytes::from((actor_id,).abi_encode_params()),
        }
    }

    const ACCOUNT: Address = address!("0x1111111111111111111111111111111111111111");
    const K1: Address = Eip8130Constants::K1_AUTHENTICATOR;
    const EXISTING_KEY: IntrinsicGasInput = IntrinsicGasInput::new(ACCOUNT, false);

    fn signed(tx: TxEip8130, sender_auth: Vec<u8>, payer_auth: Vec<u8>) -> Eip8130Signed {
        Eip8130Signed::new(tx, Bytes::from(sender_auth), Bytes::from(payer_auth))
    }

    /// `authenticator(20) || dummy data`.
    fn configured_auth(authenticator: Address) -> Vec<u8> {
        let mut blob = authenticator.to_vec();
        blob.extend_from_slice(&[0xab; 65]);
        blob
    }

    fn encode(signed: &Eip8130Signed) -> Vec<u8> {
        let mut encoded = vec![Eip8130Constants::EIP8130_TX_TYPE];
        signed.rlp_encode_signed(&mut encoded);
        encoded
    }

    /// Serializes `signed` (EIP-2718) and computes intrinsic gas, mirroring a
    /// caller that already holds the transaction's network encoding.
    fn intrinsic(signed: &Eip8130Signed, input: &IntrinsicGasInput) -> IntrinsicGas {
        IntrinsicGas::compute(signed, &encode(signed), input)
            .expect("canonical authenticators are scheduled")
    }

    alloy_sol_types::sol! {
        // Mirror of the contract's `ActorConfig` authorize payload, used only to
        // pin the byte offsets `authorize_attaches_policy` reads.
        struct ActorConfigAbi {
            address authenticator;
            uint48 expiry;
            uint16 scope;
        }
    }

    #[test]
    fn authorize_attaches_policy_reads_the_policy_length_not_the_scope_bit() {
        // Drift tripwire: attachment is length-based (base/eip-8130 #95). A
        // 52-byte `policyData` attaches regardless of scope; an empty one does
        // not attach even with SCOPE_POLICY set.
        let actor_id = B256::repeat_byte(0x11);
        let attached = (
            actor_id,
            ActorConfigAbi {
                // Scope deliberately lacks POLICY: attachment is length-only.
                authenticator: Address::ZERO,
                scope: Eip8130Constants::SCOPE_OPERATOR,
                expiry: alloy_primitives::Uint::ZERO,
            },
            Bytes::from(vec![0u8; Eip8130Constants::POLICY_DATA_LEN]),
        )
            .abi_encode_params();
        // SCOPE_POLICY set but no policy bytes -> not attached.
        let scope_only = (
            actor_id,
            ActorConfigAbi {
                authenticator: address!("0xffffffffffffffffffffffffffffffffffffffff"),
                scope: Eip8130Constants::SCOPE_POLICY,
                expiry: alloy_primitives::Uint::from(0xffff_ffff_ffffu64),
            },
            Bytes::new(),
        )
            .abi_encode_params();

        assert!(IntrinsicGas::authorize_attaches_policy(&attached));
        assert!(!IntrinsicGas::authorize_attaches_policy(&scope_only));
        // The policyData length word sits at offset 5*32; canonical offset is 0xA0.
        assert_eq!(U256::from_be_slice(&attached[128..160]), U256::from(160));
        assert_eq!(
            U256::from_be_slice(&attached[160..192]),
            U256::from(Eip8130Constants::POLICY_DATA_LEN)
        );
    }

    #[test]
    fn eoa_self_pay_minimal() {
        // sender == None (EOA), key 0 existing, no payer, no account changes.
        let tx = TxEip8130::default();
        let gas = intrinsic(&signed(tx, vec![0xcd; 65], vec![]), &EXISTING_KEY);

        assert_eq!(gas.base, Eip8130GasSchedule::AA_BASE_COST);
        assert_eq!(gas.nonce_key, Eip8130GasSchedule::NONCE_KEY_EXISTING_COST);
        assert_eq!(gas.bytecode, 0);
        assert_eq!(gas.account_changes, 0);
        // native k1 exec + 1 cold SLOAD.
        assert_eq!(
            gas.sender_auth,
            Eip8130GasSchedule::AUTH_EXEC_K1 + Eip8130GasSchedule::COLD_SLOAD
        );
        assert_eq!(gas.payer_auth, 0);
        assert!(gas.payload > 0);
        // self-pay: sender-intrinsic equals total.
        assert_eq!(gas.sender_intrinsic(), gas.total());
    }

    #[test]
    fn nonce_free_and_first_use_costs() {
        let mut tx = TxEip8130 { nonce_key: Eip8130Constants::NONCE_KEY_MAX, ..Default::default() };
        let free = intrinsic(&signed(tx.clone(), vec![0; 65], vec![]), &EXISTING_KEY);
        assert_eq!(free.nonce_key, Eip8130GasSchedule::NONCE_FREE_COST);

        tx.nonce_key = U256::from(7u64);
        let first =
            intrinsic(&signed(tx, vec![0; 65], vec![]), &IntrinsicGasInput::new(ACCOUNT, true));
        assert_eq!(first.nonce_key, Eip8130GasSchedule::NONCE_KEY_FIRST_USE_COST);
    }

    #[test]
    fn create_entry_charges_bytecode() {
        let code = vec![0x60u8; 10];
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::Create(CreateEntry {
                user_salt: Default::default(),
                code: Bytes::from(code),
                initial_actors: vec![],
            })],
            ..Default::default()
        };
        let gas = intrinsic(&signed(tx, vec![0; 65], vec![]), &EXISTING_KEY);
        assert_eq!(
            gas.bytecode,
            Eip8130GasSchedule::CREATE_BASE_COST + Eip8130GasSchedule::CODE_DEPOSIT_PER_BYTE * 10
        );
        assert_eq!(gas.account_changes, Eip8130GasSchedule::ACCOUNT_STATE_SET_COST);
    }

    #[test]
    fn create_charges_bytecode_plus_per_initial_actor_slot() {
        // A create entry pays `bytecode_cost` (base + per-byte deposit) plus one
        // fresh `actor_config` slot write per initial actor — the same per-slot
        // model as a config change authorizing the same actors.
        let code = vec![0x60u8; 4];
        let initial_actors = (0u8..3)
            .map(|i| InitialActor::owner(alloy_primitives::B256::repeat_byte(i + 1), K1))
            .collect();
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::Create(CreateEntry {
                user_salt: Default::default(),
                code: Bytes::from(code),
                initial_actors,
            })],
            ..Default::default()
        };
        let gas = intrinsic(&signed(tx, vec![0; 65], vec![]), &EXISTING_KEY);
        assert_eq!(
            gas.bytecode,
            Eip8130GasSchedule::CREATE_BASE_COST + Eip8130GasSchedule::CODE_DEPOSIT_PER_BYTE * 4
        );
        assert_eq!(
            gas.account_changes,
            Eip8130GasSchedule::ACCOUNT_STATE_SET_COST
                + Eip8130GasSchedule::ACTOR_SLOT_SET_COST * 3
                + Eip8130GasSchedule::POLICY_SLOTS_NOOP_COST * 3
        );
    }

    #[test]
    fn create_charges_three_slots_for_policy_initial_actor() {
        // A POLICY initial actor also writes `policy_manager` and
        // `policy_commitment`: 3 slot-sets versus 1 for a non-policy actor.
        let policy_data = {
            let mut d = vec![0u8; 52];
            d[19] = 0xcc; // manager
            d[51] = 0x44; // commitment
            Bytes::from(d)
        };
        let actors = vec![
            InitialActor::owner(alloy_primitives::B256::repeat_byte(1), K1),
            InitialActor {
                actor_id: alloy_primitives::B256::repeat_byte(2),
                authenticator: K1,
                scope: Eip8130Constants::SCOPE_POLICY,
                policy_data,
            },
        ];
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::Create(CreateEntry {
                user_salt: Default::default(),
                code: Bytes::from(vec![0x60u8; 4]),
                initial_actors: actors,
            })],
            ..Default::default()
        };
        let gas = intrinsic(&signed(tx, vec![0; 65], vec![]), &EXISTING_KEY);
        // Account-state bootstrap + 1 (non-policy) + 3 (policy) slot-sets.
        assert_eq!(
            gas.account_changes,
            Eip8130GasSchedule::ACCOUNT_STATE_SET_COST
                + Eip8130GasSchedule::ACTOR_SLOT_SET_COST * 4
                + Eip8130GasSchedule::POLICY_SLOTS_NOOP_COST
        );
    }

    #[test]
    fn degenerate_short_auth_blob_charges_no_sload() {
        // A sub-20-byte prefixed (non-bare) blob resolves no authenticator, so it
        // reads no `actor_config` slot and must cost 0 (not a phantom cold SLOAD).
        // A bare signature still pays the authenticator exec + one cold SLOAD.
        assert_eq!(IntrinsicGas::auth_cost(&[0u8; 5], AuthWireForm::Prefixed), Ok(0));
        assert_eq!(
            IntrinsicGas::auth_cost(&[0u8; 65], AuthWireForm::BareSignature),
            Ok(Eip8130GasSchedule::AUTH_EXEC_K1 + Eip8130GasSchedule::COLD_SLOAD)
        );
    }

    #[test]
    fn delegation_entry_charges_deposit() {
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::Delegation(Delegation { target: ACCOUNT })],
            ..Default::default()
        };
        let gas = intrinsic(&signed(tx, vec![0; 65], vec![]), &EXISTING_KEY);
        assert_eq!(gas.account_changes, Eip8130GasSchedule::DELEGATION_DEPOSIT_COST);
    }

    #[test]
    fn config_change_charges_auth_plus_slot_writes() {
        // One authorize without policy + one revoke, authorized by a configured k1.
        let authorize_data = vec![0u8; 128];
        let cc = SignedAccountChanges {
            channel: AccountChangeChannel::Multichain,
            sequence: 0,
            changes: vec![
                SignedChange {
                    change_type: ChangeType::AuthorizeActor,
                    payload: Bytes::from(authorize_data),
                },
                SignedChange { change_type: ChangeType::RevokeActor, payload: Bytes::new() },
            ],
            signature: Bytes::from(configured_auth(K1)),
        };
        let tx = TxEip8130 {
            sender: Some(ACCOUNT),
            account_changes: vec![AccountChange::ConfigChange(cc)],
            ..Default::default()
        };
        let gas = intrinsic(&signed(tx, configured_auth(K1), vec![]), &EXISTING_KEY);
        // Explicit `K1_AUTHENTICATOR` auth resolves in a single cold SLOAD (the
        // inline self config or a non-self k1 actor's `actor_config`).
        let auth_cost = Eip8130GasSchedule::AUTH_EXEC_K1 + Eip8130GasSchedule::COLD_SLOAD;
        let expected = auth_cost
            + Eip8130GasSchedule::CONFIG_CHANGE_STATE_COST
            + Eip8130GasSchedule::ACTOR_SLOT_SET_COST
            + Eip8130GasSchedule::POLICY_SLOTS_NOOP_COST
            + Eip8130GasSchedule::ACTOR_REVOKE_COST;
        assert_eq!(gas.account_changes, expected);

        // Attaching a 52-byte `policyData` makes the authorize also write the two
        // policy slots — attachment is length-based (base/eip-8130 #95), so the
        // scope bits are irrelevant here.
        let policy_payload = (
            B256::repeat_byte(0x11),
            ActorConfigAbi {
                authenticator: Address::ZERO,
                scope: Eip8130Constants::SCOPE_OPERATOR,
                expiry: alloy_primitives::Uint::ZERO,
            },
            Bytes::from(vec![0u8; Eip8130Constants::POLICY_DATA_LEN]),
        )
            .abi_encode_params();
        let cc = SignedAccountChanges {
            channel: AccountChangeChannel::Multichain,
            sequence: 0,
            changes: vec![SignedChange {
                change_type: ChangeType::AuthorizeActor,
                payload: Bytes::from(policy_payload),
            }],
            signature: Bytes::from(configured_auth(K1)),
        };
        let tx = TxEip8130 {
            sender: Some(ACCOUNT),
            account_changes: vec![AccountChange::ConfigChange(cc)],
            ..Default::default()
        };
        let gas = intrinsic(&signed(tx, configured_auth(K1), vec![]), &EXISTING_KEY);
        assert_eq!(
            gas.account_changes,
            auth_cost
                + Eip8130GasSchedule::CONFIG_CHANGE_STATE_COST
                + Eip8130GasSchedule::ACTOR_SLOT_SET_COST * 3
        );
    }

    #[test]
    fn config_change_increment_local_epoch_charges_warm_state_bump() {
        // A batch carrying a single IncrementLocalEpoch op (empty payload) pays the
        // auth + first-state cost plus the marginal warm dirty-SSTORE for rewriting
        // the packed account-state slot, and no per-actor slot writes.
        let cc = SignedAccountChanges {
            channel: AccountChangeChannel::Multichain,
            sequence: 0,
            changes: vec![SignedChange {
                change_type: ChangeType::IncrementLocalEpoch,
                payload: Bytes::new(),
            }],
            signature: Bytes::from(configured_auth(K1)),
        };
        let tx = TxEip8130 {
            sender: Some(ACCOUNT),
            account_changes: vec![AccountChange::ConfigChange(cc)],
            ..Default::default()
        };
        let gas = intrinsic(&signed(tx, configured_auth(K1), vec![]), &EXISTING_KEY);
        let auth_cost = Eip8130GasSchedule::AUTH_EXEC_K1 + Eip8130GasSchedule::COLD_SLOAD;
        assert_eq!(
            gas.account_changes,
            auth_cost
                + Eip8130GasSchedule::CONFIG_CHANGE_STATE_COST
                + Eip8130GasSchedule::INCREMENT_LOCAL_EPOCH_COST
        );
    }

    #[test]
    fn subsequent_same_account_config_changes_pay_warm_state_bump() {
        // Two config changes on the same (sender) account: the first pays the cold
        // zero-to-nonzero state write, the second only a warm SLOAD + reset because
        // the packed slot was already warmed and written by the first.
        let cc = || SignedAccountChanges {
            channel: AccountChangeChannel::Multichain,
            sequence: 0,
            changes: vec![SignedChange {
                change_type: ChangeType::AuthorizeActor,
                payload: Bytes::from(vec![0u8; 128]),
            }],
            signature: Bytes::from(configured_auth(K1)),
        };
        let one = TxEip8130 {
            sender: Some(ACCOUNT),
            account_changes: vec![AccountChange::ConfigChange(cc())],
            ..Default::default()
        };
        let two = TxEip8130 {
            sender: Some(ACCOUNT),
            account_changes: vec![
                AccountChange::ConfigChange(cc()),
                AccountChange::ConfigChange(cc()),
            ],
            ..Default::default()
        };
        let one_gas = intrinsic(&signed(one, configured_auth(K1), vec![]), &EXISTING_KEY);
        let two_gas = intrinsic(&signed(two, configured_auth(K1), vec![]), &EXISTING_KEY);

        // The second change adds one full change's worth of cost, but the state
        // component is the subsequent (warm) cost rather than another cold write.
        let per_change_non_state = Eip8130GasSchedule::AUTH_EXEC_K1
            + Eip8130GasSchedule::COLD_SLOAD
            + Eip8130GasSchedule::ACTOR_SLOT_SET_COST
            + Eip8130GasSchedule::POLICY_SLOTS_NOOP_COST;
        assert_eq!(
            two_gas.account_changes - one_gas.account_changes,
            per_change_non_state + Eip8130GasSchedule::CONFIG_CHANGE_STATE_COST_SUBSEQUENT
        );
        // The subsequent (warm) bump must be strictly cheaper than the first
        // (cold) write; a `const` assert keeps this a compile-time invariant.
        const _: () = assert!(
            Eip8130GasSchedule::CONFIG_CHANGE_STATE_COST_SUBSEQUENT
                < Eip8130GasSchedule::CONFIG_CHANGE_STATE_COST
        );
    }

    #[test]
    fn config_change_after_create_pays_warm_state_bump() {
        // A create bootstraps the packed account-state slot (cold set); a config
        // change in the same transaction then only pays the warm subsequent bump.
        let cc = SignedAccountChanges {
            channel: AccountChangeChannel::Multichain,
            sequence: 0,
            changes: vec![],
            signature: Bytes::from(configured_auth(K1)),
        };
        let tx = TxEip8130 {
            sender: Some(ACCOUNT),
            account_changes: vec![
                AccountChange::Create(CreateEntry {
                    user_salt: Default::default(),
                    code: Bytes::from(vec![0x60u8; 4]),
                    initial_actors: vec![],
                }),
                AccountChange::ConfigChange(cc),
            ],
            ..Default::default()
        };
        let gas = intrinsic(&signed(tx, configured_auth(K1), vec![]), &EXISTING_KEY);
        // Create bootstrap (cold set) + config-change auth + subsequent warm bump.
        let expected = Eip8130GasSchedule::ACCOUNT_STATE_SET_COST
            + Eip8130GasSchedule::AUTH_EXEC_K1
            + Eip8130GasSchedule::COLD_SLOAD
            + Eip8130GasSchedule::CONFIG_CHANGE_STATE_COST_SUBSEQUENT;
        assert_eq!(gas.account_changes, expected);
    }

    #[test]
    fn revoke_slot_discount_reprices_resets_to_cold_noops() {
        // A self-targeted revoke is priced at three slot resets by default. When
        // execution resolves empty zero-to-zero slots (an ungated inline secp256k1
        // self revoke has all three empty; a policy-gated one has only its
        // actor_config slot empty), each empty slot is repriced from a reset to a
        // cold zero-to-zero no-op.
        let mut bytes = [0u8; 32];
        bytes[12..].copy_from_slice(ACCOUNT.as_slice());
        let self_id = alloy_primitives::B256::from(bytes);
        let cc = || SignedAccountChanges {
            channel: AccountChangeChannel::Multichain,
            sequence: 0,
            changes: vec![revoke_op(self_id)],
            signature: Bytes::from(configured_auth(K1)),
        };
        let tx = || TxEip8130 {
            sender: Some(ACCOUNT),
            account_changes: vec![AccountChange::ConfigChange(cc())],
            ..Default::default()
        };
        let undiscounted = intrinsic(&signed(tx(), configured_auth(K1), vec![]), &EXISTING_KEY);
        // Ungated inline self: all three slots empty.
        let ungated = intrinsic(
            &signed(tx(), configured_auth(K1), vec![]),
            &EXISTING_KEY.with_revoke_discount_slots(3),
        );
        assert_eq!(
            undiscounted.account_changes - ungated.account_changes,
            3 * Eip8130GasSchedule::COLD_SLOT_RESET_DISCOUNT
        );
        // Policy-gated inline self: only the actor_config slot is empty.
        let gated = intrinsic(
            &signed(tx(), configured_auth(K1), vec![]),
            &EXISTING_KEY.with_revoke_discount_slots(1),
        );
        assert_eq!(
            undiscounted.account_changes - gated.account_changes,
            Eip8130GasSchedule::COLD_SLOT_RESET_DISCOUNT
        );
        // The per-slot discount is exactly the reset-vs-cold-noop delta.
        assert_eq!(
            Eip8130GasSchedule::COLD_SLOT_RESET_DISCOUNT,
            Eip8130GasSchedule::ACTOR_SLOT_RESET_COST - Eip8130GasSchedule::COLD_SLOT_NOOP_COST
        );
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(
        expected = "resolved revoke discount slots exceed the three-slot-per-revoke maximum"
    )]
    fn excess_revoke_discount_slots_trips_debug_assertion() {
        let mut bytes = [0u8; 32];
        bytes[12..].copy_from_slice(ACCOUNT.as_slice());
        let self_id = alloy_primitives::B256::from(bytes);
        let cc = SignedAccountChanges {
            channel: AccountChangeChannel::Multichain,
            sequence: 0,
            changes: vec![revoke_op(self_id)],
            signature: Bytes::from(configured_auth(K1)),
        };
        let tx = TxEip8130 {
            sender: Some(ACCOUNT),
            account_changes: vec![AccountChange::ConfigChange(cc)],
            ..Default::default()
        };
        let _ = intrinsic(
            &signed(tx, configured_auth(K1), vec![]),
            &EXISTING_KEY.with_revoke_discount_slots(u32::MAX),
        );
    }

    #[test]
    fn revoke_discount_slots_bounded_by_three_per_charged_revoke() {
        // One charged revoke allows at most three discounted slots.
        assert_eq!(IntrinsicGas::bounded_revoke_discount_slots(u32::MAX, 1), 3);
        assert_eq!(IntrinsicGas::bounded_revoke_discount_slots(2, 1), 2);
        // Two charged revokes allow up to six.
        assert_eq!(IntrinsicGas::bounded_revoke_discount_slots(5, 2), 5);
        assert_eq!(IntrinsicGas::bounded_revoke_discount_slots(u32::MAX, 2), 6);
    }

    #[test]
    fn self_targeted_config_change_charges_no_extra_dual_home_bump() {
        // A self-actor change writes the inline-self home (the packed
        // account_state slot), but that write is already covered by
        // `CONFIG_CHANGE_STATE_COST` (the config change's sequence bump touches the
        // same slot), and the `actor_config(self)` home is already covered by the
        // per-change `actor_change_write_cost`. So a self-targeted change costs the
        // same as a non-self change — there is no separate dual-home bump (an
        // earlier over-conservative addition that double-charged account_state).
        let mut bytes = [0u8; 32];
        bytes[12..].copy_from_slice(ACCOUNT.as_slice());
        let self_id = alloy_primitives::B256::from(bytes);
        let other_id = alloy_primitives::B256::repeat_byte(0x07);

        let account_changes = |actor_id, sender| {
            let cc = SignedAccountChanges {
                channel: AccountChangeChannel::Multichain,
                sequence: 0,
                changes: vec![SignedChange {
                    change_type: ChangeType::AuthorizeActor,
                    payload: authorize_op(actor_id, K1, 0, 0).payload,
                }],
                signature: Bytes::from(configured_auth(K1)),
            };
            let tx = TxEip8130 {
                sender,
                account_changes: vec![AccountChange::ConfigChange(cc)],
                ..Default::default()
            };
            intrinsic(&signed(tx, configured_auth(K1), vec![]), &EXISTING_KEY).account_changes
        };

        let base = Eip8130GasSchedule::AUTH_EXEC_K1
            + Eip8130GasSchedule::COLD_SLOAD
            + Eip8130GasSchedule::CONFIG_CHANGE_STATE_COST
            + Eip8130GasSchedule::ACTOR_SLOT_SET_COST
            + Eip8130GasSchedule::POLICY_SLOTS_NOOP_COST;

        // Self, non-self, and the off-wire EOA path all cost the same `base`.
        assert_eq!(account_changes(other_id, Some(ACCOUNT)), base);
        assert_eq!(account_changes(self_id, Some(ACCOUNT)), base);
        assert_eq!(account_changes(other_id, None), base);
    }

    #[test]
    fn implicit_eoa_config_auth_costs_k1() {
        // An implicit-EOA owner authorizing a config change names itself explicitly
        // as `K1_AUTHENTICATOR || sig` on the configured (`AuthWireForm::Prefixed`)
        // surface. The inline self config resolves in a single cold SLOAD.
        let auth_cost = Eip8130GasSchedule::AUTH_EXEC_K1 + Eip8130GasSchedule::COLD_SLOAD;
        assert_eq!(
            IntrinsicGas::auth_cost(&configured_auth(K1), AuthWireForm::Prefixed),
            Ok(auth_cost)
        );

        // End-to-end: a config change authorized by the implicit-EOA owner is
        // priced (not rejected), charging k1 + SLOAD plus the authorized slot.
        let cc = SignedAccountChanges {
            channel: AccountChangeChannel::Multichain,
            sequence: 0,
            changes: vec![SignedChange {
                change_type: ChangeType::AuthorizeActor,
                payload: Bytes::from(vec![0u8; 128]),
            }],
            signature: Bytes::from(configured_auth(K1)),
        };
        let tx = TxEip8130 {
            sender: Some(ACCOUNT),
            account_changes: vec![AccountChange::ConfigChange(cc)],
            ..Default::default()
        };
        let gas = intrinsic(&signed(tx, configured_auth(K1), vec![]), &EXISTING_KEY);
        assert_eq!(
            gas.account_changes,
            auth_cost
                + Eip8130GasSchedule::CONFIG_CHANGE_STATE_COST
                + Eip8130GasSchedule::ACTOR_SLOT_SET_COST
                + Eip8130GasSchedule::POLICY_SLOTS_NOOP_COST
        );
    }

    #[test]
    fn k1_authentication_costs_a_single_sload() {
        // The inline self-config model resolves any k1 authentication in one cold
        // SLOAD: a bare signature (default-EOA wire form) reads the account-state
        // slot carrying the inline self config, and an explicit `K1_AUTHENTICATOR`
        // blob reads exactly one slot too (the inline self, or a non-self k1
        // actor's `actor_config`).
        assert_eq!(
            IntrinsicGas::auth_cost(&[0u8; 65], AuthWireForm::BareSignature),
            Ok(Eip8130GasSchedule::AUTH_EXEC_K1 + Eip8130GasSchedule::COLD_SLOAD)
        );
        assert_eq!(
            IntrinsicGas::auth_cost(&configured_auth(K1), AuthWireForm::Prefixed),
            Ok(Eip8130GasSchedule::AUTH_EXEC_K1 + Eip8130GasSchedule::COLD_SLOAD)
        );
        // A non-k1 leaf actor reads only its `actor_config` slot: one cold SLOAD.
        assert_eq!(
            IntrinsicGas::auth_cost(
                &configured_auth(Eip8130Contracts::P256_AUTHENTICATOR),
                AuthWireForm::Prefixed
            ),
            Ok(Eip8130GasSchedule::AUTH_EXEC_P256 + Eip8130GasSchedule::COLD_SLOAD)
        );
    }

    #[test]
    fn zero_authenticator_selector_is_unscheduled() {
        // `address(0)` is the empty "no actor configured" sentinel, never a valid
        // authenticator selector. A configured (`AuthWireForm::Prefixed`) blob naming
        // it is rejected as unscheduled rather than silently priced as k1.
        assert_eq!(
            IntrinsicGas::auth_cost(&configured_auth(Address::ZERO), AuthWireForm::Prefixed),
            Err(IntrinsicGasError::UnscheduledAuthenticator(Address::ZERO))
        );
    }

    #[test]
    fn configured_authenticator_sender_costs() {
        for (authenticator, exec) in [
            (Eip8130Contracts::P256_AUTHENTICATOR, Eip8130GasSchedule::AUTH_EXEC_P256),
            (Eip8130Contracts::WEBAUTHN_AUTHENTICATOR, Eip8130GasSchedule::AUTH_EXEC_WEBAUTHN),
        ] {
            let tx = TxEip8130 { sender: Some(ACCOUNT), ..Default::default() };
            let gas = intrinsic(&signed(tx, configured_auth(authenticator), vec![]), &EXISTING_KEY);
            assert_eq!(gas.sender_auth, exec + Eip8130GasSchedule::COLD_SLOAD);
        }
    }

    #[test]
    fn delegate_sender_recurses_into_nested() {
        // DELEGATE || delegate_account(20) || nested_authenticator(k1) || data.
        let mut blob = Eip8130Contracts::DELEGATE_AUTHENTICATOR.to_vec();
        blob.extend_from_slice(ACCOUNT.as_slice());
        blob.extend_from_slice(K1.as_slice());
        blob.extend_from_slice(&[0xab; 65]);
        let tx = TxEip8130 { sender: Some(ACCOUNT), ..Default::default() };
        let gas = intrinsic(&signed(tx, blob, vec![]), &EXISTING_KEY);
        // delegate overhead + nested k1 exec, then the outer +1 SLOAD.
        let expected = Eip8130GasSchedule::AUTH_EXEC_DELEGATE_OVERHEAD
            + Eip8130GasSchedule::AUTH_EXEC_K1
            + Eip8130GasSchedule::COLD_SLOAD;
        assert_eq!(gas.sender_auth, expected);
    }

    #[test]
    fn unscheduled_authenticator_is_an_error() {
        // An authenticator address with no schedule entry must surface rather
        // than silently charging zero execution gas.
        let bogus = address!("0x00000000000000000000000000000000deadbeef");
        let tx = TxEip8130 { sender: Some(ACCOUNT), ..Default::default() };
        let s = signed(tx, configured_auth(bogus), vec![]);
        assert_eq!(
            IntrinsicGas::compute(&s, &encode(&s), &EXISTING_KEY),
            Err(IntrinsicGasError::UnscheduledAuthenticator(bogus))
        );
    }

    #[test]
    fn nested_delegate_is_rejected_at_depth_1() {
        // DELEGATE || delegate_account(20) || nested = DELEGATE (depth-2) || data.
        // The nested authenticator is resolved as a leaf, so a delegate there is
        // unscheduled and errors instead of recursing.
        let mut blob = Eip8130Contracts::DELEGATE_AUTHENTICATOR.to_vec();
        blob.extend_from_slice(ACCOUNT.as_slice());
        blob.extend_from_slice(Eip8130Contracts::DELEGATE_AUTHENTICATOR.as_slice());
        blob.extend_from_slice(&[0xab; 65]);
        let tx = TxEip8130 { sender: Some(ACCOUNT), ..Default::default() };
        let s = signed(tx, blob, vec![]);
        assert_eq!(
            IntrinsicGas::compute(&s, &encode(&s), &EXISTING_KEY),
            Err(IntrinsicGasError::UnscheduledAuthenticator(
                Eip8130Contracts::DELEGATE_AUTHENTICATOR
            ))
        );
    }

    #[test]
    fn sponsored_payer_is_excluded_from_sender_intrinsic() {
        let tx = TxEip8130 {
            sender: Some(ACCOUNT),
            payer: Some(address!("0x2222222222222222222222222222222222222222")),
            ..Default::default()
        };
        let payer_auth = configured_auth(Eip8130Contracts::P256_AUTHENTICATOR);
        let gas = intrinsic(&signed(tx, configured_auth(K1), payer_auth.clone()), &EXISTING_KEY);
        assert_eq!(
            gas.payer_auth,
            Eip8130GasSchedule::AUTH_EXEC_P256
                + Eip8130GasSchedule::COLD_SLOAD
                + IntrinsicGas::data_cost(&payer_auth)
        );
        // payer_auth is metered on top of gas_limit, so it is excluded here.
        assert_eq!(gas.sender_intrinsic(), gas.total() - gas.payer_auth);
        assert!(gas.payer_auth > 0);
    }

    #[test]
    fn payer_auth_bytes_do_not_change_sender_payload() {
        let tx = TxEip8130 {
            sender: Some(ACCOUNT),
            payer: Some(address!("0x2222222222222222222222222222222222222222")),
            ..Default::default()
        };
        let short =
            intrinsic(&signed(tx.clone(), configured_auth(K1), configured_auth(K1)), &EXISTING_KEY);
        let mut long_auth = configured_auth(K1);
        long_auth.extend_from_slice(&[0xff; 200]);
        let long = intrinsic(&signed(tx, configured_auth(K1), long_auth), &EXISTING_KEY);

        assert_eq!(short.payload, long.payload);
        assert_eq!(short.sender_intrinsic(), long.sender_intrinsic());
        assert_eq!(
            long.payer_auth - short.payer_auth,
            200 * Eip8130GasSchedule::TX_DATA_NONZERO_BYTE
        );
    }

    #[test]
    fn payer_auth_over_max_authentication_gas_is_an_error() {
        let tx = TxEip8130 {
            sender: Some(ACCOUNT),
            payer: Some(address!("0x2222222222222222222222222222222222222222")),
            ..Default::default()
        };
        let mut payer_auth = configured_auth(K1);
        payer_auth.extend_from_slice(&[0xff; 7_000]);
        let s = signed(tx, configured_auth(K1), payer_auth);
        assert!(matches!(
            IntrinsicGas::compute(&s, &encode(&s), &EXISTING_KEY),
            Err(IntrinsicGasError::PayerAuthGasExceeded(cost))
                if cost > Eip8130GasSchedule::MAX_AUTHENTICATION_GAS
        ));
    }

    #[test]
    fn value_calls_to_others_charge_tx_value_cost() {
        let other = address!("0x3333333333333333333333333333333333333333");
        let call = |to, value: u64| Call { to, value: U256::from(value), data: Bytes::new() };
        let tx = TxEip8130 {
            calls: vec![
                vec![call(other, 1), call(ACCOUNT, 1), call(other, 0)],
                vec![call(other, 5)],
            ],
            ..Default::default()
        };
        let gas = intrinsic(&signed(tx, vec![0; 65], vec![]), &EXISTING_KEY);
        assert_eq!(gas.value_transfer, 2 * Eip8130GasSchedule::TX_VALUE_COST);
    }

    #[test]
    fn execution_gas_available_subtracts_sender_intrinsic() {
        let tx = TxEip8130::default();
        let gas = intrinsic(&signed(tx, vec![0; 65], vec![]), &EXISTING_KEY);
        let si = gas.sender_intrinsic();
        assert_eq!(gas.execution_gas_available(si + 1_000), Some(1_000));
        assert_eq!(gas.execution_gas_available(si.saturating_sub(1)), None);
    }
}
