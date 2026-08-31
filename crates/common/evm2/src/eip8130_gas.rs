//! EIP-8130 intrinsic-gas accounting.
//!
//! A revm-free port of the reference `base-execution-eip8130` intrinsic-gas schedule and
//! computation, operating purely over the engine-neutral `base-common-consensus` EIP-8130 types.
//! It is a self-contained pure function of the signed transaction plus a few state-derived hints
//! ([`IntrinsicGasInput`]); the EIP-8130 execution engine that consumes it lands on the EIP-8130
//! track. The [`Eip8130GasSchedule`] EVM primitives are pinned to the revm reference by a parity
//! test so the two engines cannot silently diverge.

use alloy_primitives::{Address, U256};
use base_common_consensus::{
    AccountChange, ChangeType, Eip8130Constants, Eip8130Contracts, Eip8130Signed, SignedChange,
};

/// Per-component gas costs for EIP-8130 intrinsic gas (Base's current schedule).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Eip8130GasSchedule;

impl Eip8130GasSchedule {
    /// Cold `SLOAD` (EIP-2929).
    pub const COLD_SLOAD: u64 = 2_100;
    /// Warm `SLOAD` (EIP-2929).
    pub const WARM_SLOAD: u64 = 100;
    /// `SSTORE` zero → non-zero (EIP-2929).
    pub const SSTORE_SET: u64 = 20_000;
    /// `SSTORE` non-zero → non-zero (EIP-2929 warm reset).
    pub const SSTORE_RESET: u64 = 2_900;
    /// `SSTORE` to a slot already modified this transaction (EIP-2200 dirty).
    pub const SSTORE_DIRTY: u64 = Self::WARM_SLOAD;
    /// EIP-2028 zero data byte.
    pub const TX_DATA_ZERO_BYTE: u64 = 4;
    /// EIP-2028 non-zero data byte.
    pub const TX_DATA_NONZERO_BYTE: u64 = 16;

    /// Base intrinsic cost for any AA transaction.
    pub const AA_BASE_COST: u64 = Eip8130Constants::EIP8130_BASE_COST;
    /// Nonce-free (`NONCE_KEY_MAX`) replay-state cost.
    pub const NONCE_FREE_COST: u64 =
        2 * Self::COLD_SLOAD + Self::WARM_SLOAD + 3 * Self::SSTORE_RESET;
    /// First use of a sequence nonce key.
    pub const NONCE_KEY_FIRST_USE_COST: u64 = Self::COLD_SLOAD + Self::SSTORE_SET;
    /// Reuse of a sequence nonce key.
    pub const NONCE_KEY_EXISTING_COST: u64 = Self::COLD_SLOAD + Self::SSTORE_RESET;
    /// Create deployment base.
    pub const CREATE_BASE_COST: u64 = 32_000;
    /// Code deposit per byte.
    pub const CODE_DEPOSIT_PER_BYTE: u64 = 200;
    /// Delegation-indicator deposit (`0xef0100 || address`).
    pub const DELEGATION_DEPOSIT_COST: u64 =
        Self::CODE_DEPOSIT_PER_BYTE * Eip8130Constants::DELEGATION_INDICATOR_SIZE as u64;

    /// Writing a fresh actor/policy slot.
    pub const ACTOR_SLOT_SET_COST: u64 = Self::COLD_SLOAD + Self::SSTORE_SET;
    /// Overwriting an already-set actor slot.
    pub const ACTOR_SLOT_RESET_COST: u64 = Self::COLD_SLOAD + Self::SSTORE_RESET;
    /// Cold zero-to-zero slot touch.
    pub const COLD_SLOT_NOOP_COST: u64 = Self::COLD_SLOAD + Self::WARM_SLOAD;
    /// Both policy slots as zero-to-zero clears (ungated actor).
    pub const POLICY_SLOTS_NOOP_COST: u64 = Self::COLD_SLOT_NOOP_COST * 2;
    /// Initializing the packed account-state slot.
    pub const ACCOUNT_STATE_SET_COST: u64 = Self::COLD_SLOAD + Self::SSTORE_SET;
    /// First access to the packed account-state slot in a transaction.
    pub const CONFIG_CHANGE_STATE_COST: u64 = Self::ACCOUNT_STATE_SET_COST;
    /// Subsequent same-account state bump within a transaction.
    pub const CONFIG_CHANGE_STATE_COST_SUBSEQUENT: u64 = Self::WARM_SLOAD + Self::SSTORE_DIRTY;
    /// `IncrementLocalEpoch` marginal cost.
    pub const INCREMENT_LOCAL_EPOCH_COST: u64 = Self::SSTORE_DIRTY;
    /// Worst-case revoke cost (actor config + two policy slots as resets).
    pub const ACTOR_REVOKE_COST: u64 = Self::ACTOR_SLOT_RESET_COST * 3;
    /// Per-slot discount for an empty zero-to-zero revoke slot.
    pub const COLD_SLOT_RESET_DISCOUNT: u64 =
        Self::ACTOR_SLOT_RESET_COST - Self::COLD_SLOT_NOOP_COST;

    /// secp256k1 (`ECRECOVER`) authenticator execution gas.
    pub const AUTH_EXEC_K1: u64 = 3_000;
    /// P-256 (`P256VERIFY`) authenticator execution gas.
    pub const AUTH_EXEC_P256: u64 = 6_900;
    /// `WebAuthn` authenticator execution gas.
    pub const AUTH_EXEC_WEBAUTHN: u64 = 6_900;
    /// Delegate authenticator overhead (cold `actor_config` SLOAD on the delegate account).
    pub const AUTH_EXEC_DELEGATE_OVERHEAD: u64 = Self::COLD_SLOAD;

    /// Execution gas for a leaf (non-delegate) enshrined authenticator, or `None` for a
    /// non-canonical address.
    pub fn leaf_auth_exec_gas(authenticator: Address) -> Option<u64> {
        if authenticator == Eip8130Constants::K1_AUTHENTICATOR {
            Some(Self::AUTH_EXEC_K1)
        } else if authenticator == Eip8130Contracts::P256_AUTHENTICATOR {
            Some(Self::AUTH_EXEC_P256)
        } else if authenticator == Eip8130Contracts::WEBAUTHN_AUTHENTICATOR {
            Some(Self::AUTH_EXEC_WEBAUTHN)
        } else {
            None
        }
    }
}

/// The wire form of an auth blob: a bare signature (empty-`sender` EOA path) or an
/// `authenticator || data` blob (every other surface).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthWireForm {
    /// A raw 65-byte secp256k1 signature with no authenticator prefix.
    BareSignature,
    /// An `authenticator(20) || data` blob.
    Prefixed,
}

impl AuthWireForm {
    /// The wire form of a transaction's `sender_auth` given its (optional) configured sender.
    pub const fn for_sender(sender: Option<Address>) -> Self {
        match sender {
            Some(_) => Self::Prefixed,
            None => Self::BareSignature,
        }
    }
}

/// Reason intrinsic gas cannot be computed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntrinsicGasError {
    /// An authenticator address has no gas-schedule entry.
    UnscheduledAuthenticator(Address),
}

impl core::fmt::Display for IntrinsicGasError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnscheduledAuthenticator(a) => {
                write!(f, "no gas-schedule entry for authenticator {a}")
            }
        }
    }
}

impl core::error::Error for IntrinsicGasError {}

/// State-derived inputs the transaction body alone cannot determine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntrinsicGasInput {
    /// Whether the sequence nonce channel is being used for the first time.
    pub nonce_key_first_use: bool,
    /// Whether a code-less `sender` EOA is auto-delegated during block execution.
    pub sender_auto_delegated: bool,
    /// Whether sender authorization resolved a policy-bearing actor.
    pub sender_policy_gated: bool,
    /// Whether payer authorization resolved a policy-bearing actor.
    pub payer_policy_gated: bool,
    /// Number of revoke slots execution resolved to be empty zero-to-zero touches.
    pub revoke_discount_slots: u32,
}

impl IntrinsicGasInput {
    /// Creates the intrinsic-gas state hints.
    pub const fn new(nonce_key_first_use: bool, sender_auto_delegated: bool) -> Self {
        Self {
            nonce_key_first_use,
            sender_auto_delegated,
            sender_policy_gated: false,
            payer_policy_gated: false,
            revoke_discount_slots: 0,
        }
    }

    /// Adds the policy-gate state resolved during sender and payer authorization.
    pub const fn with_policy_gates(mut self, sender: bool, payer: bool) -> Self {
        self.sender_policy_gated = sender;
        self.payer_policy_gated = payer;
        self
    }

    /// Adds the count of empty zero-to-zero revoke slots resolved during execution.
    pub const fn with_revoke_discount_slots(mut self, slots: u32) -> Self {
        self.revoke_discount_slots = slots;
        self
    }

    /// Body-derivable classifier for [`Self::sender_auto_delegated`]: the sender is auto-delegated
    /// unless the transaction carries a `Delegation` or `Create` account change.
    pub fn sender_auto_delegated(account_changes: &[AccountChange]) -> bool {
        !account_changes
            .iter()
            .any(|change| matches!(change, AccountChange::Delegation(_) | AccountChange::Create(_)))
    }
}

/// The EIP-8130 intrinsic-gas breakdown, one field per spec component.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IntrinsicGas {
    /// `AA_BASE_COST`.
    pub base: u64,
    /// EIP-2028 data-availability cost.
    pub payload: u64,
    /// `nonce_key_cost`.
    pub nonce_key: u64,
    /// Account-creation bytecode cost.
    pub bytecode: u64,
    /// Config-change and delegation entry cost.
    pub account_changes: u64,
    /// Code-less sender auto-delegation cost.
    pub auto_delegation: u64,
    /// Sender authenticator execution + authorize SLOAD(s).
    pub sender_auth: u64,
    /// Payer authenticator execution + authorize SLOAD(s), or `0` for self-pay.
    pub payer_auth: u64,
}

impl IntrinsicGas {
    /// Total intrinsic gas across all components.
    pub const fn total(&self) -> u64 {
        self.base
            .saturating_add(self.payload)
            .saturating_add(self.nonce_key)
            .saturating_add(self.bytecode)
            .saturating_add(self.account_changes)
            .saturating_add(self.auto_delegation)
            .saturating_add(self.sender_auth)
            .saturating_add(self.payer_auth)
    }

    /// Sender-intrinsic gas: total minus the payer-auth cost (metered on top of `gas_limit`).
    pub const fn sender_intrinsic(&self) -> u64 {
        self.total().saturating_sub(self.payer_auth)
    }

    /// Gas available to `calls` after sender-intrinsic gas, or `None` when the transaction is
    /// underfunded.
    pub const fn execution_gas_available(&self, gas_limit: u64) -> Option<u64> {
        gas_limit.checked_sub(self.sender_intrinsic())
    }

    /// Computes the intrinsic gas for a signed EIP-8130 transaction. `encoded` is its EIP-2718
    /// serialization (fed to the EIP-2028 payload cost).
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
        let mut account_state_touched = false;
        for change in &tx.account_changes {
            match change {
                AccountChange::Create(entry) => {
                    let deposit = Eip8130GasSchedule::CODE_DEPOSIT_PER_BYTE
                        .saturating_mul(u64::try_from(entry.code.len()).unwrap_or(u64::MAX));
                    bytecode = bytecode
                        .saturating_add(Eip8130GasSchedule::CREATE_BASE_COST)
                        .saturating_add(deposit);
                    account_changes =
                        account_changes.saturating_add(Eip8130GasSchedule::ACCOUNT_STATE_SET_COST);
                    account_state_touched = true;
                    // Each initial actor writes one fresh actor-config slot, plus the two policy
                    // slots when it attaches a 52-byte `policyData` — a policy initial actor is 3
                    // slot-sets versus 1 for a non-policy actor. Attachment is length-based
                    // (base/eip-8130 #95), decoupled from `SCOPE_POLICY`.
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
                    let state_cost = if account_state_touched {
                        Eip8130GasSchedule::CONFIG_CHANGE_STATE_COST_SUBSEQUENT
                    } else {
                        account_state_touched = true;
                        Eip8130GasSchedule::CONFIG_CHANGE_STATE_COST
                    };
                    account_changes = account_changes.saturating_add(state_cost);
                    let auth =
                        Self::auth_cost(cc.signature.as_ref(), AuthWireForm::Prefixed, false)?;
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

        let discounted_slots =
            Self::bounded_revoke_discount_slots(input.revoke_discount_slots, revoke_change_count);
        let revoke_discount = Eip8130GasSchedule::COLD_SLOT_RESET_DISCOUNT
            .saturating_mul(u64::from(discounted_slots));
        account_changes = account_changes.saturating_sub(revoke_discount);

        let auto_delegation = if input.sender_auto_delegated {
            Eip8130GasSchedule::DELEGATION_DEPOSIT_COST
        } else {
            0
        };

        let sender_auth = Self::auth_cost(
            signed.sender_auth().as_ref(),
            AuthWireForm::for_sender(tx.sender),
            input.sender_policy_gated,
        )?;
        let payer_auth = if tx.payer.is_some() {
            Self::auth_cost(
                signed.payer_auth().as_ref(),
                AuthWireForm::Prefixed,
                input.payer_policy_gated,
            )?
        } else {
            0
        };

        Ok(Self {
            base: Eip8130GasSchedule::AA_BASE_COST,
            payload: Self::payload_cost(encoded),
            nonce_key,
            bytecode,
            account_changes,
            auto_delegation,
            sender_auth,
            payer_auth,
        })
    }

    /// Conservative upper bound on the payer-authentication gas billed on top of `gas_limit`
    /// (`0` for self-pay), pinning the payer's policy-gate worst case.
    pub fn max_payer_auth_cost(signed: &Eip8130Signed) -> Result<u64, IntrinsicGasError> {
        if signed.tx().payer.is_some() {
            let auth =
                Self::auth_cost(signed.payer_auth().as_ref(), AuthWireForm::Prefixed, false)?;
            Ok(auth.saturating_add(Eip8130GasSchedule::COLD_SLOAD))
        } else {
            Ok(0)
        }
    }

    /// Bounds an execution-resolved empty-slot discount by three slots per revoke.
    const fn bounded_revoke_discount_slots(reported: u32, revoke_change_count: u32) -> u32 {
        let max = revoke_change_count.saturating_mul(3);
        if reported < max { reported } else { max }
    }

    /// EIP-2028 data-availability cost over the serialized transaction.
    fn payload_cost(encoded: &[u8]) -> u64 {
        encoded.iter().fold(0u64, |acc, &byte| {
            let cost = if byte == 0 {
                Eip8130GasSchedule::TX_DATA_ZERO_BYTE
            } else {
                Eip8130GasSchedule::TX_DATA_NONZERO_BYTE
            };
            acc.saturating_add(cost)
        })
    }

    /// Cost of authenticating one auth blob: authenticator execution gas plus the cold SLOADs the
    /// `authorize` step reads.
    fn auth_cost(
        auth: &[u8],
        form: AuthWireForm,
        policy_gated: bool,
    ) -> Result<u64, IntrinsicGasError> {
        let exec = Self::auth_exec_cost(auth, form)?;
        let sloads =
            Self::auth_sloads(auth, form, exec).saturating_add(u64::from(policy_gated && exec > 0));
        Ok(exec.saturating_add(Eip8130GasSchedule::COLD_SLOAD.saturating_mul(sloads)))
    }

    /// Number of cold SLOADs the `authorize` step reads for one authentication.
    fn auth_sloads(auth: &[u8], form: AuthWireForm, exec: u64) -> u64 {
        if matches!(form, AuthWireForm::BareSignature) {
            return 1;
        }
        match Self::authenticator_of(auth) {
            Some(_) if exec > 0 => 1,
            _ => 0,
        }
    }

    /// Authenticator execution gas, resolving the delegate authenticator's nested authenticator at
    /// depth-1.
    fn auth_exec_cost(auth: &[u8], form: AuthWireForm) -> Result<u64, IntrinsicGasError> {
        if matches!(form, AuthWireForm::BareSignature) {
            return Ok(Eip8130GasSchedule::AUTH_EXEC_K1);
        }
        let Some(authenticator) = Self::authenticator_of(auth) else {
            return Ok(0);
        };
        if authenticator == Eip8130Contracts::DELEGATE_AUTHENTICATOR {
            let nested_exec = match auth.get(40..).and_then(Self::authenticator_of) {
                Some(nested) => Self::leaf_exec_gas(nested)?,
                None => 0,
            };
            return Ok(Eip8130GasSchedule::AUTH_EXEC_DELEGATE_OVERHEAD.saturating_add(nested_exec));
        }
        Self::leaf_exec_gas(authenticator)
    }

    /// Execution gas for a leaf (non-delegate) enshrined authenticator.
    fn leaf_exec_gas(authenticator: Address) -> Result<u64, IntrinsicGasError> {
        Eip8130GasSchedule::leaf_auth_exec_gas(authenticator)
            .ok_or(IntrinsicGasError::UnscheduledAuthenticator(authenticator))
    }

    /// The authenticator address at the head of a configured-actor auth blob.
    fn authenticator_of(auth: &[u8]) -> Option<Address> {
        (auth.len() >= 20).then(|| Address::from_slice(&auth[..20]))
    }

    /// Storage-write cost for one actor change.
    fn actor_change_write_cost(op: &SignedChange) -> u64 {
        match op.change_type {
            ChangeType::RevokeActor => Eip8130GasSchedule::ACTOR_REVOKE_COST,
            ChangeType::AuthorizeActor => {
                let mut cost = Eip8130GasSchedule::ACTOR_SLOT_SET_COST;
                if Self::authorize_attaches_policy(op.payload.as_ref()) {
                    cost = cost
                        .saturating_add(Eip8130GasSchedule::ACTOR_SLOT_SET_COST.saturating_mul(2));
                } else {
                    cost = cost.saturating_add(Eip8130GasSchedule::POLICY_SLOTS_NOOP_COST);
                }
                cost
            }
            ChangeType::IncrementLocalEpoch => Eip8130GasSchedule::INCREMENT_LOCAL_EPOCH_COST,
            ChangeType::Lock | ChangeType::Unlock => 0,
        }
    }

    /// Whether an authorize op's ABI-encoded payload attaches a policy — i.e. carries a 52-byte
    /// `policyData`. Attachment is length-based (base/eip-8130 #95), decoupled from `SCOPE_POLICY`.
    /// The params encoding is four static head words (`actorId`, `authenticator`, `expiry`,
    /// `scope`) then the `policyData` offset word; a pointer out of range is metered as no policy.
    fn authorize_attaches_policy(payload: &[u8]) -> bool {
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
    use alloy_primitives::{B256, Bytes, address};
    use alloy_sol_types::SolValue;
    use base_common_consensus::{
        AccountChange, AccountChangeChannel, ChangeType, CreateEntry, Delegation, InitialActor,
        SignedAccountChanges, SignedChange, TxEip8130,
    };

    use super::*;

    const ACCOUNT: Address = address!("0x1111111111111111111111111111111111111111");
    const K1: Address = Eip8130Constants::K1_AUTHENTICATOR;

    fn signed(tx: TxEip8130, sender_auth: Vec<u8>, payer_auth: Vec<u8>) -> Eip8130Signed {
        Eip8130Signed::new(tx, Bytes::from(sender_auth), Bytes::from(payer_auth))
    }

    /// `authenticator(20) || dummy data`.
    fn configured_auth(authenticator: Address) -> Vec<u8> {
        let mut blob = authenticator.to_vec();
        blob.extend_from_slice(&[0xab; 65]);
        blob
    }

    /// The evm2 schedule constants must equal the revm reference's, so the two engines can never
    /// silently diverge on intrinsic-gas pricing.
    #[test]
    fn schedule_constants_match_the_revm_reference() {
        use base_execution_eip8130::Eip8130GasSchedule as Ref;
        assert_eq!(Eip8130GasSchedule::COLD_SLOAD, Ref::COLD_SLOAD);
        assert_eq!(Eip8130GasSchedule::WARM_SLOAD, Ref::WARM_SLOAD);
        assert_eq!(Eip8130GasSchedule::SSTORE_SET, Ref::SSTORE_SET);
        assert_eq!(Eip8130GasSchedule::SSTORE_RESET, Ref::SSTORE_RESET);
        assert_eq!(Eip8130GasSchedule::AA_BASE_COST, Ref::AA_BASE_COST);
        assert_eq!(Eip8130GasSchedule::NONCE_FREE_COST, Ref::NONCE_FREE_COST);
        assert_eq!(Eip8130GasSchedule::DELEGATION_DEPOSIT_COST, Ref::DELEGATION_DEPOSIT_COST);
        assert_eq!(Eip8130GasSchedule::ACTOR_REVOKE_COST, Ref::ACTOR_REVOKE_COST);
        assert_eq!(Eip8130GasSchedule::AUTH_EXEC_K1, Ref::AUTH_EXEC_K1);
        assert_eq!(Eip8130GasSchedule::AUTH_EXEC_P256, Ref::AUTH_EXEC_P256);
    }

    /// The full computation must match the revm reference across a range of transaction shapes.
    #[test]
    fn compute_matches_the_revm_reference() {
        use base_execution_eip8130::{IntrinsicGas as RefGas, IntrinsicGasInput as RefInput};

        let authorize = SignedChange {
            change_type: ChangeType::AuthorizeActor,
            payload: Bytes::from(
                (B256::repeat_byte(0x01), configured_auth(K1), Bytes::new()).abi_encode_params(),
            ),
        };
        let revoke = SignedChange {
            change_type: ChangeType::RevokeActor,
            payload: Bytes::from((B256::repeat_byte(0x02),).abi_encode_params()),
        };

        let cases: Vec<(TxEip8130, Vec<u8>, Vec<u8>)> = vec![
            // Simple EOA-sender transfer (bare signature), self-pay.
            (TxEip8130 { gas_limit: 100_000, ..Default::default() }, vec![0x11; 65], vec![]),
            // Configured sender + payer.
            (
                TxEip8130 {
                    gas_limit: 200_000,
                    sender: Some(ACCOUNT),
                    payer: Some(ACCOUNT),
                    ..Default::default()
                },
                configured_auth(K1),
                configured_auth(K1),
            ),
            // Nonce-free replay transaction.
            (
                TxEip8130 {
                    gas_limit: 100_000,
                    nonce_key: Eip8130Constants::NONCE_KEY_MAX,
                    ..Default::default()
                },
                vec![0x22; 65],
                vec![],
            ),
            // Create entry with a policy actor + a config change with authorize/revoke.
            (
                TxEip8130 {
                    gas_limit: 500_000,
                    sender: Some(ACCOUNT),
                    account_changes: vec![
                        AccountChange::Create(CreateEntry {
                            user_salt: B256::ZERO,
                            code: Bytes::from(vec![0x60; 32]),
                            initial_actors: vec![InitialActor {
                                actor_id: B256::ZERO,
                                authenticator: K1,
                                scope: Eip8130Constants::SCOPE_POLICY,
                                policy_data: Bytes::new(),
                            }],
                        }),
                        AccountChange::ConfigChange(SignedAccountChanges {
                            channel: AccountChangeChannel::Local,
                            sequence: 0,
                            signature: Bytes::from(configured_auth(K1)),
                            changes: vec![authorize, revoke],
                        }),
                        AccountChange::Delegation(Delegation { target: Address::ZERO }),
                    ],
                    ..Default::default()
                },
                configured_auth(K1),
                vec![],
            ),
        ];

        for (i, (tx, sender_auth, payer_auth)) in cases.into_iter().enumerate() {
            let signed_tx = signed(tx, sender_auth, payer_auth);
            let encoded = vec![0x79u8, 0x00, 0xff, 0x00, 0xab, 0x00];
            let auto = IntrinsicGasInput::sender_auto_delegated(&signed_tx.tx().account_changes);
            let input = IntrinsicGasInput::new(true, auto).with_policy_gates(true, false);
            let ref_input = RefInput::new(true, auto).with_policy_gates(true, false);

            let ours = IntrinsicGas::compute(&signed_tx, &encoded, &input).expect("evm2 computes");
            let theirs = RefGas::compute(&signed_tx, &encoded, &ref_input).expect("revm computes");

            assert_eq!(ours.total(), theirs.total(), "intrinsic total diverged for case {i}");
            assert_eq!(
                ours.sender_intrinsic(),
                theirs.sender_intrinsic(),
                "sender-intrinsic diverged for case {i}",
            );
            assert_eq!(ours.payer_auth, theirs.payer_auth, "payer-auth diverged for case {i}");
        }
    }
}
