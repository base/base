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

/// The EIP-8130 intrinsic-gas breakdown, one field per spec component.
///
/// Intrinsic gas is a pure function of the signed transaction and its resolved
/// sender: it reads no state. Costs that depend on state the body cannot show
/// (whether the sender's actor has a policy, whether a revoked slot was already
/// empty, whether the packed account-state slot was already written) are
/// priced at their worst case, so estimation, mempool admission and execution
/// all compute the same value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct IntrinsicGas {
    /// `AA_BASE_COST`.
    pub base: u64,
    /// `tx_payload_cost` — EIP-2028 data-availability cost (the *standard*
    /// per-token rate) over the RLP of `account_changes`, `calls` and
    /// `metadata`, plus `sender_auth` unless it is a k1 signature, which
    /// [`Self::sender_auth`] already covers. Subject to the EIP-7623 calldata
    /// floor; see [`Self::payload_floor`] and [`Self::sender_floor`].
    pub payload: u64,
    /// The EIP-7623 calldata *floor* over the same bytes [`Self::payload`]
    /// covers: `TX_TOTAL_COST_FLOOR_PER_TOKEN` (10) per payload token instead of
    /// the standard `TX_DATA_ZERO_BYTE` (4). Always `>= payload`. It only raises
    /// the metered gas via [`Self::sender_floor`] when a data-heavy
    /// transaction's execution is cheap.
    pub payload_floor: u64,
    /// `first_call_cost` — [`Eip8130GasSchedule::COLD_ACCOUNT_ACCESS`] for the
    /// first call's target, as EIP-2780 charges `tx.to`. `0` when `calls` is
    /// empty or the first call targets the sender. Dispatch then charges the
    /// first call only for its EIP-7702 delegate.
    pub first_call: u64,
    /// `nonce_key_cost`.
    pub nonce_key: u64,
    /// `value_transfer_cost` — [`Eip8130GasSchedule::TX_VALUE_COST`] per call
    /// with `value > 0` and `to != sender`.
    pub value_transfer: u64,
    /// `bytecode_cost` — account creation.
    pub bytecode: u64,
    /// `account_changes_cost` — config-change and delegation entries.
    pub account_changes: u64,
    /// `sender_auth_cost` — sender authenticator execution and its `authorize`
    /// SLOAD (see `auth_sloads`).
    pub sender_auth: u64,
    /// `payer_auth_cost` — payer authenticator execution and its `authorize`
    /// SLOAD, plus the `payer_auth` bytes at the EIP-7623 floor rate
    /// (`TX_TOTAL_COST_FLOOR_PER_TOKEN` per token) unless they are a k1
    /// signature. `0` for self-pay. Billed to the payer on top of `gas_limit`.
    pub payer_auth_cost: u64,
}

impl IntrinsicGas {
    /// Sender-intrinsic gas: every component except payer authentication, which
    /// is metered on top of `gas_limit`. This is the portion bounded by
    /// `gas_limit`.
    #[must_use]
    pub const fn sender_intrinsic(&self) -> u64 {
        self.base
            .saturating_add(self.payload)
            .saturating_add(self.nonce_key)
            .saturating_add(self.value_transfer)
            .saturating_add(self.bytecode)
            .saturating_add(self.account_changes)
            .saturating_add(self.sender_auth)
            .saturating_add(self.first_call)
    }

    /// Gas available to `calls` after sender-intrinsic gas, or `None` when
    /// sender-intrinsic gas alone exceeds `gas_limit` (the transaction is
    /// underfunded and cannot be included).
    ///
    /// The EIP-7623 calldata floor ([`Self::sender_floor`]) does not reduce this
    /// budget — it is a post-execution minimum spend, not an intrinsic cost — so
    /// the gas available to `calls` is still `gas_limit - sender_intrinsic`. The
    /// floor is enforced separately as a lower bound on `gas_limit` and on the
    /// settled charge.
    #[must_use]
    pub const fn execution_gas_available(&self, gas_limit: u64) -> Option<u64> {
        gas_limit.checked_sub(self.sender_intrinsic())
    }

    /// The EIP-7623 sender-side floor: the minimum gas a transaction is metered
    /// against its `gas_limit`, regardless of how little its `calls` execute.
    /// As in EIP-2780, the floor base keeps the base, authentication, recipient
    /// and value charges and drops the state-dependent ones (`nonce_key`,
    /// `bytecode`, `account_changes`):
    ///
    /// ```text
    /// sender_floor = base + sender_auth + first_call + value_transfer + payload_floor
    /// ```
    ///
    /// A `gas_limit` below this is invalid, and settlement charges at least
    /// `sender_floor + payer_auth_cost`.
    #[must_use]
    pub const fn sender_floor(&self) -> u64 {
        self.base
            .saturating_add(self.sender_auth)
            .saturating_add(self.first_call)
            .saturating_add(self.value_transfer)
            .saturating_add(self.payload_floor)
    }

    /// Computes the intrinsic gas for a signed EIP-8130 transaction.
    ///
    /// Returns [`IntrinsicGasError::UnscheduledAuthenticator`] if any sender,
    /// payer, or config-change authenticator lacks a gas-schedule entry, and
    /// [`IntrinsicGasError::PayerAuthGasExceeded`] if `payer_auth_cost` exceeds
    /// [`Eip8130GasSchedule::MAX_AUTHENTICATION_GAS`].
    ///
    /// `sender` is the resolved sender: calls to it carry no recipient or value
    /// charge.
    #[must_use = "discarding the result silently skips the entire intrinsic-gas computation"]
    pub fn compute(signed: &Eip8130Signed, sender: Address) -> Result<Self, IntrinsicGasError> {
        let tx = signed.tx();

        // A sequenced key's current nonce must equal `nonce_sequence`, so a zero
        // sequence is the key's first use.
        let nonce_key = if tx.nonce_key.is_zero() {
            Eip8130GasSchedule::PROTOCOL_NONCE_COST
        } else if tx.nonce_key == Eip8130Constants::NONCE_KEY_MAX {
            Eip8130GasSchedule::NONCE_FREE_COST
        } else if tx.nonce_sequence == 0 {
            Eip8130GasSchedule::NONCE_KEY_FIRST_USE_COST
        } else {
            Eip8130GasSchedule::NONCE_KEY_EXISTING_COST
        };

        let mut bytecode = 0u64;
        let mut account_changes = 0u64;
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
                    account_changes =
                        account_changes.saturating_add(Eip8130GasSchedule::ACCOUNT_STATE_SET_COST);
                    // Each initial actor writes one fresh `actor_config` slot, plus
                    // the two policy slots (`policy_manager` + `policy_commitment`)
                    // when it attaches a 52-byte `policyData` — a policy initial
                    // actor is 3 slot-sets versus 1 for a non-policy actor.
                    // Attachment is length-based and decoupled from `SCOPE_POLICY`.
                    // These slot writes are metered per actor, mirroring the
                    // `ConfigChange` per-slot accounting below — creation must not
                    // register actors for free relative to a later config change
                    // authorizing the same set.
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
                    // channel. Every config change pays a cold zero-to-nonzero
                    // write, even when an earlier entry already wrote the slot.
                    account_changes = account_changes
                        .saturating_add(Eip8130GasSchedule::CONFIG_CHANGE_STATE_COST);
                    // `signature` is always `authenticator || data` (never a bare
                    // signature); an implicit-EOA owner names itself explicitly as
                    // `K1_AUTHENTICATOR || sig` here.
                    let auth = Self::auth_cost(cc.signature.as_ref(), AuthWireForm::Prefixed)?;
                    account_changes = account_changes.saturating_add(auth);
                    for op in &cc.changes {
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

        // Only the empty-`sender` path (`sender == None`) is a bare 65-byte
        // signature parsed via native ecrecover; a configured sender (and every
        // payer) is an `authenticator || data` blob and must not be parsed as a
        // bare signature.
        let sender_auth_bytes = signed.sender_auth().as_ref();
        let sender_form = AuthWireForm::for_sender(tx.sender);
        let sender_auth = Self::auth_cost(sender_auth_bytes, sender_form)?;
        let payer_auth_cost = Self::payer_auth_cost_of(signed)?;

        let first_call = match tx.calls.iter().flatten().next() {
            Some(call) if call.to != sender => Eip8130GasSchedule::COLD_ACCOUNT_ACCESS,
            _ => 0,
        };
        let value_calls = tx
            .calls
            .iter()
            .flatten()
            .filter(|call| !call.value.is_zero() && call.to != sender)
            .count();
        let value_transfer = Eip8130GasSchedule::TX_VALUE_COST
            .saturating_mul(u64::try_from(value_calls).unwrap_or(u64::MAX));

        let (mut payload, mut payload_floor) = Self::data_costs_of_counts(
            signed.metered_payload_zero_bytes(),
            signed.metered_payload_len() - signed.metered_payload_zero_bytes(),
        );
        let (auth_standard, auth_floor) =
            Self::metered_auth_data_costs(sender_auth_bytes, sender_form);
        payload = payload.saturating_add(auth_standard);
        payload_floor = payload_floor.saturating_add(auth_floor);

        Ok(Self {
            base: Eip8130GasSchedule::AA_BASE_COST,
            payload,
            payload_floor,
            first_call,
            nonce_key,
            value_transfer,
            bytecode,
            account_changes,
            sender_auth,
            payer_auth_cost,
        })
    }

    /// The payer-authentication gas ([`Self::payer_auth_cost`]) billed to the
    /// payer on top of `gas_limit` for a signed EIP-8130 transaction (`0` for
    /// self-pay).
    ///
    /// Block gas reservation uses this to budget the payer's authentication in
    /// addition to the sender-signed `gas_limit`: the payer reimburses its own
    /// authentication beyond that limit, so a block admitting a transaction on
    /// `gas_limit` alone could let true consumption push cumulative gas over the
    /// block limit. It depends only on the payer fields, so building,
    /// validation and settlement share this exact value.
    ///
    /// Returns [`IntrinsicGasError::PayerAuthGasExceeded`] when it exceeds
    /// [`Eip8130GasSchedule::MAX_AUTHENTICATION_GAS`].
    #[must_use = "discarding the result skips the payer-authentication reservation"]
    pub fn payer_auth_cost_of(signed: &Eip8130Signed) -> Result<u64, IntrinsicGasError> {
        if signed.tx().payer.is_none() {
            return Ok(0);
        }
        let payer_auth = signed.payer_auth().as_ref();
        let form = if signed.tx().is_open_payer() {
            AuthWireForm::BareSignature
        } else {
            AuthWireForm::Prefixed
        };
        let auth = Self::auth_cost(payer_auth, form)?;
        let (_, floor_data) = Self::metered_auth_data_costs(payer_auth, form);
        let payer_auth_cost = auth.saturating_add(floor_data);
        if payer_auth_cost > Eip8130GasSchedule::MAX_AUTHENTICATION_GAS {
            return Err(IntrinsicGasError::PayerAuthGasExceeded(payer_auth_cost));
        }
        Ok(payer_auth_cost)
    }

    /// Standard and floor data costs of an auth blob's bytes, or `(0, 0)` for
    /// a k1 signature (bare or `K1_AUTHENTICATOR`-prefixed), whose bytes the
    /// k1 authentication cost covers as EIP-2780's base covers an ECDSA
    /// signature.
    fn metered_auth_data_costs(auth: &[u8], form: AuthWireForm) -> (u64, u64) {
        let k1 = matches!(form, AuthWireForm::BareSignature)
            || Self::authenticator_of(auth) == Some(Eip8130Constants::K1_AUTHENTICATOR);
        if k1 { (0, 0) } else { Self::data_costs(auth) }
    }

    /// Standard EIP-2028 and EIP-7623 floor data costs of `bytes`.
    fn data_costs(bytes: &[u8]) -> (u64, u64) {
        let zero_bytes = bytes.iter().filter(|byte| **byte == 0).count();
        Self::data_costs_of_counts(zero_bytes, bytes.len() - zero_bytes)
    }

    /// Standard EIP-2028 and EIP-7623 floor data costs of a payload with the
    /// given byte counts. A zero byte is one token and a non-zero byte is four.
    fn data_costs_of_counts(zero_bytes: usize, nonzero_bytes: usize) -> (u64, u64) {
        let zero = u64::try_from(zero_bytes).unwrap_or(u64::MAX);
        let nonzero = u64::try_from(nonzero_bytes).unwrap_or(u64::MAX);
        let standard = zero
            .saturating_mul(Eip8130GasSchedule::TX_DATA_ZERO_BYTE)
            .saturating_add(nonzero.saturating_mul(Eip8130GasSchedule::TX_DATA_NONZERO_BYTE));
        let tokens = zero.saturating_add(nonzero.saturating_mul(4));
        (standard, tokens.saturating_mul(Eip8130GasSchedule::TX_TOTAL_COST_FLOOR_PER_TOKEN))
    }

    /// Cost of authenticating one auth blob: authenticator execution gas plus the
    /// cold SLOADs the `authorize` step reads. The sender's `policy_manager`
    /// read is in [`Eip8130GasSchedule::AA_BASE_COST`].
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
    /// and decoupled from `SCOPE_POLICY`, matching the slots execution actually
    /// writes.
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

    const ACCOUNT: Address = address!("0x1111111111111111111111111111111111111111");
    const K1: Address = Eip8130Constants::K1_AUTHENTICATOR;
    /// A k1 authentication: ecrecover and its config SLOAD.
    const K1_AUTH: u64 = Eip8130GasSchedule::AUTH_EXEC_K1 + Eip8130GasSchedule::COLD_SLOAD;

    fn signed(tx: TxEip8130, sender_auth: Vec<u8>, payer_auth: Vec<u8>) -> Eip8130Signed {
        Eip8130Signed::new(tx, Bytes::from(sender_auth), Bytes::from(payer_auth))
    }

    /// `authenticator(20) || dummy data`.
    fn configured_auth(authenticator: Address) -> Vec<u8> {
        let mut blob = authenticator.to_vec();
        blob.extend_from_slice(&[0xab; 65]);
        blob
    }

    fn intrinsic(signed: &Eip8130Signed) -> IntrinsicGas {
        IntrinsicGas::compute(signed, ACCOUNT).expect("canonical authenticators are scheduled")
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
        // Attachment is length-based: a 52-byte `policyData` attaches regardless
        // of scope; an empty one does not attach even with `SCOPE_POLICY` set.
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
        // sender == None (EOA), protocol nonce (key 0), no payer, no account changes.
        let tx = TxEip8130::default();
        let gas = intrinsic(&signed(tx, vec![0xcd; 65], vec![]));

        assert_eq!(gas.base, Eip8130GasSchedule::AA_BASE_COST);
        assert_eq!(gas.nonce_key, 0);
        assert_eq!(gas.bytecode, 0);
        assert_eq!(gas.account_changes, 0);
        assert_eq!(gas.sender_auth, K1_AUTH);
        assert_eq!(gas.first_call, 0);
        assert_eq!(gas.payer_auth_cost, 0);
        assert!(gas.payload > 0);
    }

    #[test]
    fn eth_transfer_costs_more_than_eip2780() {
        // A self-paid k1 value transfer costs more than the same transfer under
        // EIP-2780 before its payload bytes, so an AA transaction is never
        // cheaper than the equivalent EIP-1559 one.
        const EIP2780_TX_BASE_COST: u64 = 12_000;
        let other = address!("0x3333333333333333333333333333333333333333");
        let tx = TxEip8130 {
            calls: vec![vec![Call { to: other, value: U256::from(1u64), data: Bytes::new() }]],
            ..Default::default()
        };
        let gas = intrinsic(&signed(tx, vec![0xcd; 65], vec![]));
        let eip2780 = EIP2780_TX_BASE_COST
            + Eip8130GasSchedule::COLD_ACCOUNT_ACCESS
            + Eip8130GasSchedule::TX_VALUE_COST;
        assert!(gas.sender_intrinsic() - gas.payload > eip2780);
    }

    #[test]
    fn first_call_target_is_charged_cold_unless_it_is_the_sender() {
        let other = address!("0x3333333333333333333333333333333333333333");
        let call = |to| Call { to, value: U256::ZERO, data: Bytes::new() };
        let first_call = |calls| {
            let tx = TxEip8130 { calls, ..Default::default() };
            intrinsic(&signed(tx, vec![0xcd; 65], vec![])).first_call
        };

        assert_eq!(first_call(vec![]), 0);
        assert_eq!(first_call(vec![vec![call(ACCOUNT)], vec![call(other)]]), 0);
        // Only the first call is charged, and empty phases are skipped.
        assert_eq!(
            first_call(vec![vec![], vec![call(other), call(other)]]),
            Eip8130GasSchedule::COLD_ACCOUNT_ACCESS
        );
    }

    #[test]
    fn payload_meters_the_data_fields_but_not_the_envelope() {
        let base = TxEip8130 { sender: Some(ACCOUNT), ..Default::default() };
        let payload =
            |tx: TxEip8130, sender_auth| intrinsic(&signed(tx, sender_auth, vec![])).payload;
        let k1 = payload(base.clone(), configured_auth(K1));

        let envelope = TxEip8130 {
            gas_limit: u64::MAX,
            max_fee_per_gas: u128::MAX,
            nonce_sequence: u64::MAX,
            ..base.clone()
        };
        assert_eq!(payload(envelope, configured_auth(K1)), k1);

        let with_metadata = TxEip8130 { metadata: Bytes::from(vec![0xff; 10]), ..base.clone() };
        assert_eq!(
            payload(with_metadata, configured_auth(K1)) - k1,
            10 * Eip8130GasSchedule::TX_DATA_NONZERO_BYTE
        );

        let p256_auth = configured_auth(Eip8130Contracts::P256_AUTHENTICATOR);
        assert_eq!(payload(base, p256_auth.clone()) - k1, IntrinsicGas::data_costs(&p256_auth).0);
    }

    #[test]
    fn sender_floor_excludes_state_dependent_costs() {
        // Nonce-free replay state and a delegation raise sender-intrinsic gas
        // but move the floor only by the bytes they add to the payload.
        let plain = intrinsic(&signed(TxEip8130::default(), vec![0xcd; 65], vec![]));
        let tx = TxEip8130 {
            nonce_key: Eip8130Constants::NONCE_KEY_MAX,
            account_changes: vec![AccountChange::Delegation(Delegation { target: ACCOUNT })],
            ..Default::default()
        };
        let stateful = intrinsic(&signed(tx, vec![0xcd; 65], vec![]));
        assert_eq!(
            stateful.sender_floor() - plain.sender_floor(),
            stateful.payload_floor - plain.payload_floor
        );
        assert!(stateful.sender_floor() < stateful.sender_intrinsic());
    }

    #[test]
    fn eip7623_calldata_floor_prices_payload_above_the_standard_rate() {
        // The floor swaps the standard EIP-2028 4/16-per-byte rate for EIP-7623's
        // 10/40-per-token rate over the same serialized bytes, so `payload_floor`
        // is exactly `TX_TOTAL_COST_FLOOR_PER_TOKEN` per token and the sender floor
        // sits `(10 - 4)` gas per token above sender-intrinsic gas.
        let gas = intrinsic(&signed(TxEip8130::default(), vec![0xcd; 65], vec![]));

        assert!(gas.payload > 0);
        // Standard payload == TX_DATA_ZERO_BYTE (4) per token; recover the token
        // count from it, then check the floor is the per-token floor rate.
        assert_eq!(gas.payload % Eip8130GasSchedule::TX_DATA_ZERO_BYTE, 0);
        let tokens = gas.payload / Eip8130GasSchedule::TX_DATA_ZERO_BYTE;
        assert_eq!(gas.payload_floor, Eip8130GasSchedule::TX_TOTAL_COST_FLOOR_PER_TOKEN * tokens);
        assert!(gas.payload_floor > gas.payload);

        // With no state-dependent costs, the floor exceeds sender-intrinsic gas
        // by exactly (10 - 4) gas per token.
        let per_token_premium = Eip8130GasSchedule::TX_TOTAL_COST_FLOOR_PER_TOKEN
            - Eip8130GasSchedule::TX_DATA_ZERO_BYTE;
        assert_eq!(gas.sender_floor() - gas.sender_intrinsic(), per_token_premium * tokens);
        assert!(gas.sender_floor() >= gas.sender_intrinsic());
    }

    #[test]
    fn eip7623_floor_grows_with_payload_size() {
        // A data-heavy transaction pays a strictly larger floor premium than a
        // minimal one: the floor is what stops an 8130 transaction from posting
        // data availability more cheaply than a standard EIP-7623 transaction.
        let minimal = intrinsic(&signed(TxEip8130::default(), vec![0xcd; 65], vec![]));
        let heavy_tx =
            TxEip8130 { metadata: Bytes::from(vec![0x11u8; 20_000]), ..Default::default() };
        let heavy = intrinsic(&signed(heavy_tx, vec![0xcd; 65], vec![]));

        assert!(heavy.payload > minimal.payload);
        assert!(
            heavy.sender_floor() - heavy.sender_intrinsic()
                > minimal.sender_floor() - minimal.sender_intrinsic()
        );
    }

    #[test]
    fn nonce_key_cost_follows_the_signed_nonce_fields() {
        let nonce_key = |nonce_key, nonce_sequence| {
            let tx = TxEip8130 { nonce_key, nonce_sequence, ..Default::default() };
            intrinsic(&signed(tx, vec![0; 65], vec![])).nonce_key
        };

        assert_eq!(
            nonce_key(Eip8130Constants::NONCE_KEY_MAX, 0),
            Eip8130GasSchedule::NONCE_FREE_COST
        );
        // A sequenced key's nonce must equal `nonce_sequence`, so sequence 0 is
        // its first use.
        assert_eq!(nonce_key(U256::from(7u64), 0), Eip8130GasSchedule::NONCE_KEY_FIRST_USE_COST);
        assert_eq!(nonce_key(U256::from(7u64), 3), Eip8130GasSchedule::NONCE_KEY_EXISTING_COST);
        // Key 0 is the account nonce: execution bumps it without touching the
        // nonce manager, so neither a fresh nor a used account pays for a slot.
        assert_eq!(nonce_key(U256::ZERO, 0), Eip8130GasSchedule::PROTOCOL_NONCE_COST);
        assert_eq!(nonce_key(U256::ZERO, 3), Eip8130GasSchedule::PROTOCOL_NONCE_COST);
    }

    #[test]
    fn k1_payer_auth_bytes_are_not_metered() {
        let tx = TxEip8130 { payer: Some(Address::repeat_byte(0x22)), ..Default::default() };
        let gas = intrinsic(&signed(tx, vec![0xcd; 65], configured_auth(K1)));
        assert_eq!(
            gas.payer_auth_cost,
            Eip8130GasSchedule::AUTH_EXEC_K1 + Eip8130GasSchedule::COLD_SLOAD
        );
    }

    #[test]
    fn non_k1_payer_auth_bytes_are_billed_at_the_floor_rate() {
        let tx = TxEip8130 { payer: Some(Address::repeat_byte(0x22)), ..Default::default() };
        let payer_auth = configured_auth(Eip8130Contracts::P256_AUTHENTICATOR);
        let zeros = payer_auth.iter().filter(|byte| **byte == 0).count() as u64;
        let non_zeros = payer_auth.len() as u64 - zeros;
        let sponsored = signed(tx, vec![0xcd; 65], payer_auth);
        let gas = intrinsic(&sponsored);

        assert_eq!(
            gas.payer_auth_cost,
            Eip8130GasSchedule::AUTH_EXEC_P256
                + Eip8130GasSchedule::COLD_SLOAD
                + Eip8130GasSchedule::TX_TOTAL_COST_FLOOR_PER_TOKEN * (zeros + 4 * non_zeros)
        );
        assert_eq!(IntrinsicGas::payer_auth_cost_of(&sponsored), Ok(gas.payer_auth_cost));
        let self_pay = signed(TxEip8130::default(), vec![0xcd; 65], vec![]);
        assert_eq!(IntrinsicGas::payer_auth_cost_of(&self_pay), Ok(0));
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
        let gas = intrinsic(&signed(tx, vec![0; 65], vec![]));
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
        let gas = intrinsic(&signed(tx, vec![0; 65], vec![]));
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
        let gas = intrinsic(&signed(tx, vec![0; 65], vec![]));
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
        let gas = intrinsic(&signed(tx, vec![0; 65], vec![]));
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
        let gas = intrinsic(&signed(tx, configured_auth(K1), vec![]));
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
        // policy slots — attachment is length-based, so the scope bits are
        // irrelevant here.
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
        let gas = intrinsic(&signed(tx, configured_auth(K1), vec![]));
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
        let gas = intrinsic(&signed(tx, configured_auth(K1), vec![]));
        let auth_cost = Eip8130GasSchedule::AUTH_EXEC_K1 + Eip8130GasSchedule::COLD_SLOAD;
        assert_eq!(
            gas.account_changes,
            auth_cost
                + Eip8130GasSchedule::CONFIG_CHANGE_STATE_COST
                + Eip8130GasSchedule::INCREMENT_LOCAL_EPOCH_COST
        );
    }

    #[test]
    fn every_config_change_pays_the_cold_state_write() {
        // A second config change on the same account pays the full cold state
        // write again, even though the first already wrote the packed slot.
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
        let one_gas = intrinsic(&signed(one, configured_auth(K1), vec![]));
        let two_gas = intrinsic(&signed(two, configured_auth(K1), vec![]));

        assert_eq!(two_gas.account_changes, 2 * one_gas.account_changes);
    }

    #[test]
    fn config_change_after_create_pays_the_cold_state_write() {
        // A create bootstraps the packed account-state slot; a config change in
        // the same transaction still pays its own cold state write.
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
        let gas = intrinsic(&signed(tx, configured_auth(K1), vec![]));
        let expected = Eip8130GasSchedule::ACCOUNT_STATE_SET_COST
            + Eip8130GasSchedule::AUTH_EXEC_K1
            + Eip8130GasSchedule::COLD_SLOAD
            + Eip8130GasSchedule::CONFIG_CHANGE_STATE_COST;
        assert_eq!(gas.account_changes, expected);
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
            intrinsic(&signed(tx, configured_auth(K1), vec![])).account_changes
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
        let gas = intrinsic(&signed(tx, configured_auth(K1), vec![]));
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
                AuthWireForm::Prefixed,
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
            let gas = intrinsic(&signed(tx, configured_auth(authenticator), vec![]));
            assert_eq!(gas.sender_auth, exec + Eip8130GasSchedule::COLD_SLOAD);
        }
    }

    #[test]
    fn sender_and_payer_k1_authentication_cost_the_same() {
        let tx = TxEip8130 {
            sender: Some(ACCOUNT),
            payer: Some(Address::repeat_byte(0x22)),
            ..Default::default()
        };
        let gas = intrinsic(&signed(tx, configured_auth(K1), configured_auth(K1)));
        assert_eq!(gas.sender_auth, K1_AUTH);
        assert_eq!(gas.payer_auth_cost, K1_AUTH);
    }

    #[test]
    fn delegate_sender_recurses_into_nested() {
        // DELEGATE || delegate_account(20) || nested_authenticator(k1) || data.
        let mut blob = Eip8130Contracts::DELEGATE_AUTHENTICATOR.to_vec();
        blob.extend_from_slice(ACCOUNT.as_slice());
        blob.extend_from_slice(K1.as_slice());
        blob.extend_from_slice(&[0xab; 65]);
        let tx = TxEip8130 { sender: Some(ACCOUNT), ..Default::default() };
        let gas = intrinsic(&signed(tx, blob, vec![]));
        // delegate overhead + nested k1 exec, then the outer config SLOAD.
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
            IntrinsicGas::compute(&s, ACCOUNT),
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
            IntrinsicGas::compute(&s, ACCOUNT),
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
        let gas = intrinsic(&signed(tx.clone(), configured_auth(K1), payer_auth));
        // payer_auth is metered on top of gas_limit, so a costlier payer leaves
        // the sender-intrinsic portion unchanged.
        let k1_payer = intrinsic(&signed(tx, configured_auth(K1), configured_auth(K1)));
        assert_ne!(k1_payer.payer_auth_cost, gas.payer_auth_cost);
        assert_eq!(k1_payer.sender_intrinsic(), gas.sender_intrinsic());
        assert_eq!(k1_payer.sender_floor(), gas.sender_floor());
    }

    #[test]
    fn open_payer_auth_is_priced_as_a_bare_k1_signature() {
        let tx = TxEip8130 { payer: Some(Eip8130Constants::OPEN_PAYER), ..Default::default() };
        let gas = intrinsic(&signed(tx, vec![0; 65], vec![0xab; 65]));
        assert_eq!(
            gas.payer_auth_cost,
            Eip8130GasSchedule::AUTH_EXEC_K1 + Eip8130GasSchedule::COLD_SLOAD
        );
    }

    #[test]
    fn payer_auth_bytes_do_not_change_sender_payload() {
        let tx = TxEip8130 {
            sender: Some(ACCOUNT),
            payer: Some(address!("0x2222222222222222222222222222222222222222")),
            ..Default::default()
        };
        let p256 = configured_auth(Eip8130Contracts::P256_AUTHENTICATOR);
        let short = intrinsic(&signed(tx.clone(), configured_auth(K1), p256.clone()));
        let mut long_auth = p256;
        long_auth.extend_from_slice(&[0xff; 200]);
        let long = intrinsic(&signed(tx, configured_auth(K1), long_auth));

        assert_eq!(short.payload, long.payload);
        assert_eq!(short.sender_intrinsic(), long.sender_intrinsic());
        assert_eq!(
            long.payer_auth_cost - short.payer_auth_cost,
            200 * 4 * Eip8130GasSchedule::TX_TOTAL_COST_FLOOR_PER_TOKEN,
            "payer_auth bytes are billed at the calldata floor rate"
        );
    }

    #[test]
    fn payer_auth_over_max_authentication_gas_is_an_error() {
        let tx = TxEip8130 {
            sender: Some(ACCOUNT),
            payer: Some(address!("0x2222222222222222222222222222222222222222")),
            ..Default::default()
        };
        let mut payer_auth = configured_auth(Eip8130Contracts::P256_AUTHENTICATOR);
        payer_auth.extend_from_slice(&[0xff; 7_000]);
        let s = signed(tx, configured_auth(K1), payer_auth);
        assert!(matches!(
            IntrinsicGas::compute(&s, ACCOUNT),
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
        let gas = intrinsic(&signed(tx, vec![0; 65], vec![]));
        assert_eq!(gas.value_transfer, 2 * Eip8130GasSchedule::TX_VALUE_COST);
    }

    #[test]
    fn execution_gas_available_subtracts_sender_intrinsic() {
        let tx = TxEip8130::default();
        let gas = intrinsic(&signed(tx, vec![0; 65], vec![]));
        let si = gas.sender_intrinsic();
        assert_eq!(gas.execution_gas_available(si + 1_000), Some(1_000));
        assert_eq!(gas.execution_gas_available(si.saturating_sub(1)), None);
    }
}
