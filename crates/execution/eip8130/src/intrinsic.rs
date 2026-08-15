//! EIP-8130 intrinsic gas: the total cost to include an AA transaction.

use alloy_primitives::Address;
use base_common_consensus::{
    AccountChange, ActorChange, ActorChangeType, Eip8130Constants, Eip8130Contracts, Eip8130Signed,
};

use crate::{AccountConfigurationStorage, Eip8130GasSchedule};

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
/// Both flags come from the caller's state view (the nonce manager / account
/// state and the sender's code), supplied so this crate stays a pure function of
/// the transaction plus these hints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct IntrinsicGasInput {
    /// Whether this transaction's sequence nonce channel is being used for the
    /// first time (its current nonce is zero) — selects the SSTORE *set* cost
    /// over the *reset* cost. Ignored for nonce-free (`NONCE_KEY_MAX`)
    /// transactions.
    pub nonce_key_first_use: bool,
    /// Whether a code-less `sender` EOA is auto-delegated to `DEFAULT_ACCOUNT`
    /// during block execution, incurring the delegation-indicator deposit.
    pub sender_auto_delegated: bool,
}

impl IntrinsicGasInput {
    /// Creates the intrinsic-gas state hints.
    #[must_use]
    pub const fn new(nonce_key_first_use: bool, sender_auto_delegated: bool) -> Self {
        Self { nonce_key_first_use, sender_auto_delegated }
    }
}

/// The EIP-8130 intrinsic-gas breakdown, one field per spec component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct IntrinsicGas {
    /// `AA_BASE_COST`.
    pub base: u64,
    /// `tx_payload_cost` — EIP-2028 data-availability cost.
    pub payload: u64,
    /// `nonce_key_cost`.
    pub nonce_key: u64,
    /// `bytecode_cost` — account creation.
    pub bytecode: u64,
    /// `account_changes_cost` — config-change and delegation entries.
    pub account_changes: u64,
    /// `auto_delegation_cost` — code-less sender auto-delegation.
    pub auto_delegation: u64,
    /// `sender_auth_cost` — sender authenticator execution + its `authorize`
    /// SLOAD(s) (see `auth_sloads`).
    pub sender_auth: u64,
    /// `payer_auth_cost` — payer authenticator execution + its `authorize`
    /// SLOAD(s), or `0` for self-pay.
    pub payer_auth: u64,
}

impl IntrinsicGas {
    /// Total intrinsic gas (all components).
    #[must_use]
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
    /// EIP-2028 `payload` cost.
    ///
    /// Returns [`IntrinsicGasError::UnscheduledAuthenticator`] if any sender,
    /// payer, or config-change authenticator lacks a gas-schedule entry.
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
        for change in &tx.account_changes {
            match change {
                AccountChange::Create(entry) => {
                    // `bytecode_cost`: deployment base + per-byte code deposit.
                    let deposit = Eip8130GasSchedule::CODE_DEPOSIT_PER_BYTE
                        .saturating_mul(u64::try_from(entry.code.len()).unwrap_or(u64::MAX));
                    bytecode = bytecode
                        .saturating_add(Eip8130GasSchedule::CREATE_BASE_COST)
                        .saturating_add(deposit);
                    // Each initial actor writes one fresh `actor_config` slot (an
                    // unrestricted owner: `scope = 0`, `expiry = 0`,
                    // `policyType = 0`, so no policy slots). These slot writes are
                    // metered per actor, mirroring the `ConfigChange` per-slot
                    // accounting below — creation must not register actors for
                    // free relative to a later config change authorizing the same
                    // set.
                    let initial_actor_cost = Eip8130GasSchedule::ACTOR_SLOT_SET_COST
                        .saturating_mul(
                            u64::try_from(entry.initial_actors.len()).unwrap_or(u64::MAX),
                        );
                    account_changes = account_changes.saturating_add(initial_actor_cost);
                }
                AccountChange::ConfigChange(cc) => {
                    // `cfg.auth` is always `authenticator || data` (never a bare
                    // signature); an implicit-EOA owner names itself explicitly as
                    // `K1_AUTHENTICATOR || sig` here.
                    let auth = Self::auth_cost(cc.auth.as_ref(), AuthWireForm::Prefixed)?;
                    account_changes = account_changes.saturating_add(auth);
                    for actor_change in &cc.actor_changes {
                        account_changes = account_changes
                            .saturating_add(Self::actor_change_write_cost(actor_change));
                    }
                    account_changes = account_changes
                        .saturating_add(Self::self_actor_change_cost(tx.sender, &cc.actor_changes));
                }
                AccountChange::Delegation(_) => {
                    account_changes =
                        account_changes.saturating_add(Eip8130GasSchedule::DELEGATION_DEPOSIT_COST);
                }
            }
        }

        let auto_delegation = if input.sender_auto_delegated {
            Eip8130GasSchedule::DELEGATION_DEPOSIT_COST
        } else {
            0
        };

        // Only the empty-`sender` path (`sender == None`) is a bare 65-byte
        // signature parsed via native ecrecover; a configured sender (and every
        // payer) is an `authenticator || data` blob and must not be parsed as a
        // bare signature.
        let sender_auth =
            Self::auth_cost(signed.sender_auth().as_ref(), AuthWireForm::for_sender(tx.sender))?;
        let payer_auth = if tx.payer.is_some() {
            Self::auth_cost(signed.payer_auth().as_ref(), AuthWireForm::Prefixed)?
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

    /// EIP-2028 data-availability cost over the caller-supplied EIP-2718
    /// serialization (`type_byte || rlp([..fields.., sender_auth, payer_auth])`).
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
    /// a revoke overwrites the existing slot.
    fn actor_change_write_cost(actor_change: &ActorChange) -> u64 {
        match actor_change.change_type {
            ActorChangeType::Revoke => Eip8130GasSchedule::ACTOR_SLOT_RESET_COST,
            ActorChangeType::Authorize => {
                let mut cost = Eip8130GasSchedule::ACTOR_SLOT_SET_COST;
                if Self::authorize_has_policy(actor_change.data.as_ref()) {
                    // policy_commitment + policy_manager.
                    cost = cost
                        .saturating_add(Eip8130GasSchedule::ACTOR_SLOT_SET_COST.saturating_mul(2));
                }
                cost
            }
        }
    }

    /// Whether an authorize's ABI-encoded `(ActorConfig, bytes)` `data` carries a
    /// non-zero `policyType`. `ActorConfig` is `(address, uint8 scope, uint48
    /// expiry, uint8 policyType)`, so `policyType` is the fourth 32-byte word.
    fn authorize_has_policy(data: &[u8]) -> bool {
        data.len() >= 128 && data[96..128].iter().any(|&byte| byte != 0)
    }

    /// Worst-case extra cost for self-targeted actor changes in a config change.
    ///
    /// A change to the account's own secp256k1 self-actor is a dual-home write:
    /// the self key's config lives inline in the account-state slot, and the
    /// change also touches the mutually-exclusive `actor_config(self)` home — a
    /// second storage home a non-self actor change never writes. `tx.sender` is
    /// the config-change account when wire-visible, so each self-targeted change
    /// is matched exactly. On the empty-`sender` (EOA) path the account is
    /// recovered off-wire and can't be matched here; the self-actorId is unique,
    /// so at most one change can target it and a single worst-case bump covers
    /// the config change.
    fn self_actor_change_cost(sender: Option<Address>, actor_changes: &[ActorChange]) -> u64 {
        let self_changes = sender.map_or_else(
            || u64::from(!actor_changes.is_empty()),
            |account| {
                // Delegate to the canonical self-actor id (the authorize layer's
                // `AccountConfigurationStorage::self_actor_id`) so the gas layer and
                // the authorize layer can never match different actors.
                let self_id = AccountConfigurationStorage::self_actor_id(account);
                let count = actor_changes.iter().filter(|c| c.actor_id == self_id).count();
                u64::try_from(count).unwrap_or(u64::MAX)
            },
        );
        Eip8130GasSchedule::SELF_ACTOR_DUAL_HOME_COST.saturating_mul(self_changes)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, Bytes, U256, address};
    use base_common_consensus::{
        AccountChange, ActorChange, ActorChangeType, ConfigChange, CreateEntry, Delegation,
        InitialActor, TxEip8130,
    };

    use super::*;

    const ACCOUNT: Address = address!("0x1111111111111111111111111111111111111111");
    const K1: Address = Eip8130Constants::K1_AUTHENTICATOR;
    const EXISTING_KEY: IntrinsicGasInput = IntrinsicGasInput::new(false, false);

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
        // pin the byte offset `authorize_has_policy` reads.
        struct ActorConfigAbi {
            address authenticator;
            uint8 scope;
            uint48 expiry;
            uint8 policyType;
        }
    }

    #[test]
    fn authorize_has_policy_reads_the_policy_type_word() {
        use alloy_sol_types::SolValue;

        // Drift tripwire: `authorize_has_policy` decodes `policyType` at a hardcoded
        // 32-byte offset (bytes 96..128) of the ABI-encoded `(ActorConfig, bytes)`
        // authorize payload. If the `ActorConfig` field layout ever changes, this
        // catches it: a non-zero `policyType` must be detected, and non-zero values
        // in *every other* field (authenticator, scope, expiry) must not be.
        let gated = (
            ActorConfigAbi {
                authenticator: Address::ZERO,
                scope: 0,
                expiry: alloy_primitives::Uint::ZERO,
                policyType: 7,
            },
            Bytes::new(),
        )
            .abi_encode_params();
        let ungated = (
            ActorConfigAbi {
                authenticator: address!("0xffffffffffffffffffffffffffffffffffffffff"),
                scope: 0xff,
                expiry: alloy_primitives::Uint::from(0xffff_ffff_ffffu64),
                policyType: 0,
            },
            Bytes::new(),
        )
            .abi_encode_params();

        assert!(IntrinsicGas::authorize_has_policy(&gated));
        assert!(!IntrinsicGas::authorize_has_policy(&ungated));
        // `policyType = 7` lands in the low byte of the fourth word.
        assert_eq!(gated[96..128].iter().rposition(|&b| b != 0), Some(31));
        assert_eq!(gated[127], 7);
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
        assert_eq!(gas.auto_delegation, 0);
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
            intrinsic(&signed(tx, vec![0; 65], vec![]), &IntrinsicGasInput::new(true, false));
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
    }

    #[test]
    fn create_charges_bytecode_plus_per_initial_actor_slot() {
        // A create entry pays `bytecode_cost` (base + per-byte deposit) plus one
        // fresh `actor_config` slot write per initial actor — the same per-slot
        // model as a config change authorizing the same actors (no policy slots,
        // since initial actors are unrestricted owners).
        let code = vec![0x60u8; 4];
        let initial_actors = (0u8..3)
            .map(|i| InitialActor {
                actor_id: alloy_primitives::B256::repeat_byte(i + 1),
                authenticator: K1,
            })
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
        assert_eq!(gas.account_changes, Eip8130GasSchedule::ACTOR_SLOT_SET_COST * 3);
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
        let mut authorize_data = vec![0u8; 128];
        let cc = ConfigChange {
            chain_id: 0,
            sequence: 0,
            actor_changes: vec![
                ActorChange {
                    change_type: ActorChangeType::Authorize,
                    actor_id: Default::default(),
                    data: Bytes::from(authorize_data.clone()),
                },
                ActorChange {
                    change_type: ActorChangeType::Revoke,
                    actor_id: Default::default(),
                    data: Bytes::new(),
                },
            ],
            auth: Bytes::from(configured_auth(K1)),
        };
        let tx = TxEip8130 {
            sender: Some(ACCOUNT),
            account_changes: vec![AccountChange::ConfigChange(cc)],
            ..Default::default()
        };
        let gas = intrinsic(&signed(tx, configured_auth(K1), vec![]), &EXISTING_KEY);
        // Explicit `K1_AUTHENTICATOR` auth resolves in a single cold SLOAD (the
        // inline self config or a non-self k1 actor's `actor_config`). The actor
        // ids are non-self, so no self-actor dual-home bump applies.
        let auth_cost = Eip8130GasSchedule::AUTH_EXEC_K1 + Eip8130GasSchedule::COLD_SLOAD;
        let expected = auth_cost
            + Eip8130GasSchedule::ACTOR_SLOT_SET_COST
            + Eip8130GasSchedule::ACTOR_SLOT_RESET_COST;
        assert_eq!(gas.account_changes, expected);

        // With a non-zero policyType, the authorize also writes the two policy slots.
        authorize_data[127] = 0x01;
        let cc = ConfigChange {
            chain_id: 0,
            sequence: 0,
            actor_changes: vec![ActorChange {
                change_type: ActorChangeType::Authorize,
                actor_id: Default::default(),
                data: Bytes::from(authorize_data),
            }],
            auth: Bytes::from(configured_auth(K1)),
        };
        let tx = TxEip8130 {
            sender: Some(ACCOUNT),
            account_changes: vec![AccountChange::ConfigChange(cc)],
            ..Default::default()
        };
        let gas = intrinsic(&signed(tx, configured_auth(K1), vec![]), &EXISTING_KEY);
        assert_eq!(gas.account_changes, auth_cost + Eip8130GasSchedule::ACTOR_SLOT_SET_COST * 3);
    }

    #[test]
    fn self_targeted_config_change_charges_dual_home_bump() {
        // bytes32(bytes20(account)) — the account's own self-actor id.
        let mut bytes = [0u8; 32];
        bytes[..20].copy_from_slice(ACCOUNT.as_slice());
        let self_id = alloy_primitives::B256::from(bytes);
        let other_id = alloy_primitives::B256::repeat_byte(0x07);

        let account_changes = |actor_id, sender| {
            let cc = ConfigChange {
                chain_id: 0,
                sequence: 0,
                actor_changes: vec![ActorChange {
                    change_type: ActorChangeType::Authorize,
                    actor_id,
                    data: Bytes::from(vec![0u8; 128]),
                }],
                auth: Bytes::from(configured_auth(K1)),
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
            + Eip8130GasSchedule::ACTOR_SLOT_SET_COST;

        // Configured sender targeting a non-self actor: no dual-home bump.
        assert_eq!(account_changes(other_id, Some(ACCOUNT)), base);
        // Configured sender targeting its own self-actor: + dual-home bump.
        assert_eq!(
            account_changes(self_id, Some(ACCOUNT)),
            base + Eip8130GasSchedule::SELF_ACTOR_DUAL_HOME_COST
        );
        // EOA sender (`sender == None`): the account is off-wire, so the unique
        // self-actorId can't be matched and a single worst-case bump is charged.
        assert_eq!(
            account_changes(other_id, None),
            base + Eip8130GasSchedule::SELF_ACTOR_DUAL_HOME_COST
        );
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
        let cc = ConfigChange {
            chain_id: 0,
            sequence: 0,
            actor_changes: vec![ActorChange {
                change_type: ActorChangeType::Authorize,
                actor_id: Default::default(),
                data: Bytes::from(vec![0u8; 128]),
            }],
            auth: Bytes::from(configured_auth(K1)),
        };
        let tx = TxEip8130 {
            sender: Some(ACCOUNT),
            account_changes: vec![AccountChange::ConfigChange(cc)],
            ..Default::default()
        };
        let gas = intrinsic(&signed(tx, configured_auth(K1), vec![]), &EXISTING_KEY);
        assert_eq!(gas.account_changes, auth_cost + Eip8130GasSchedule::ACTOR_SLOT_SET_COST);
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
        let gas = intrinsic(
            &signed(tx, configured_auth(K1), configured_auth(Eip8130Contracts::P256_AUTHENTICATOR)),
            &EXISTING_KEY,
        );
        assert_eq!(
            gas.payer_auth,
            Eip8130GasSchedule::AUTH_EXEC_P256 + Eip8130GasSchedule::COLD_SLOAD
        );
        // payer_auth is metered on top of gas_limit, so it is excluded here.
        assert_eq!(gas.sender_intrinsic(), gas.total() - gas.payer_auth);
        assert!(gas.payer_auth > 0);
    }

    #[test]
    fn auto_delegation_adds_indicator_deposit() {
        let tx = TxEip8130::default();
        let gas = intrinsic(&signed(tx, vec![0; 65], vec![]), &IntrinsicGasInput::new(false, true));
        assert_eq!(gas.auto_delegation, Eip8130GasSchedule::DELEGATION_DEPOSIT_COST);
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
