//! Stateless structural rules for an EIP-8130 transaction.
//!
//! Shared by pool admission and RPC simulation, so a request the pool would
//! reject as malformed is not priced or simulated either.

use alloy_primitives::Address;

use super::{
    AccountChange, ChangeType, Eip8130Constants, Eip8130Contracts, Eip8130Signed, InitialActor,
    SignedChange,
};

/// Why an EIP-8130 transaction fails the stateless structural rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Eip8130StructuralError {
    /// More call phases than [`Eip8130Constants::MAX_CALL_PHASES_PER_TX`].
    #[error("call phase count exceeds maximum")]
    TooManyCallPhases,
    /// `sender_auth` has the wrong shape for its path or names a disallowed
    /// authenticator.
    #[error("malformed sender authorization")]
    MalformedSenderAuth,
    /// `payer_auth` is missing, unexpected, or has the wrong shape for its mode.
    #[error("malformed payer authorization")]
    MalformedPayerAuth,
    /// More account changes than [`Eip8130Constants::MAX_ACCOUNT_CHANGES_PER_TX`].
    #[error("account change count exceeds maximum")]
    TooManyAccountChanges,
    /// An account change entry violates its structural invariants.
    #[error("malformed account change")]
    MalformedAccountChange,
}

/// The stateless structural rules for an EIP-8130 transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Eip8130Structure;

impl Eip8130Structure {
    /// Checks every structural rule: the call phase count, the sender and payer
    /// authorization shapes, and the account changes.
    pub fn validate(signed: &Eip8130Signed) -> Result<(), Eip8130StructuralError> {
        if signed.tx().calls.len() > Eip8130Constants::MAX_CALL_PHASES_PER_TX {
            return Err(Eip8130StructuralError::TooManyCallPhases);
        }
        Self::validate_sender_auth(signed)?;
        Self::validate_payer_auth(signed)?;
        Self::validate_account_changes(signed)
    }

    /// Checks the `sender_auth` field carries enough bytes for either the EOA
    /// recovery path (65-byte signature) or the configured-actor auth path
    /// (`authenticator_address || authenticator_payload`) and that the authenticator address
    /// is not the sentinel revoked marker.
    pub fn validate_sender_auth(signed: &Eip8130Signed) -> Result<(), Eip8130StructuralError> {
        let auth = signed.sender_auth();
        if auth.is_empty() {
            return Err(Eip8130StructuralError::MalformedSenderAuth);
        }
        if signed.explicit_sender().is_none() {
            // EOA path: must carry exactly the secp256k1 signature.
            if auth.len() != 65 {
                return Err(Eip8130StructuralError::MalformedSenderAuth);
            }
        } else {
            // Configured-actor path: leading 20 bytes are the authenticator address.
            if auth.len() < 20 {
                return Err(Eip8130StructuralError::MalformedSenderAuth);
            }
            let authenticator = Address::from_slice(&auth[..20]);
            if !Self::authenticator_allowed_for_tx_path(&authenticator)
                || !Self::authenticator_payload_well_formed(&authenticator, &auth[20..])
            {
                return Err(Eip8130StructuralError::MalformedSenderAuth);
            }
        }
        Ok(())
    }

    /// Ensures `payer_auth` is present iff a `payer` is set. Open payer mode
    /// carries a raw 65-byte signature; a named payer carries an allowed
    /// `authenticator || data` blob.
    pub fn validate_payer_auth(signed: &Eip8130Signed) -> Result<(), Eip8130StructuralError> {
        let payer_present = signed.tx().payer.is_some();
        let auth = signed.payer_auth();
        // XOR: presence must match.
        if payer_present == auth.is_empty() {
            return Err(Eip8130StructuralError::MalformedPayerAuth);
        }
        if signed.tx().is_open_payer() {
            if auth.len() != 65 {
                return Err(Eip8130StructuralError::MalformedPayerAuth);
            }
        } else if payer_present {
            if auth.len() < 20 {
                return Err(Eip8130StructuralError::MalformedPayerAuth);
            }
            let authenticator = Address::from_slice(&auth[..20]);
            if !Self::authenticator_allowed_for_tx_path(&authenticator)
                || !Self::authenticator_payload_well_formed(&authenticator, &auth[20..])
            {
                return Err(Eip8130StructuralError::MalformedPayerAuth);
            }
        }
        Ok(())
    }

    /// Returns `true` when `authenticator` falls outside the live mempool policy
    /// range. Mirrors the check in [`Self::validate_initial_actors`] and
    /// [`Self::validate_actor_changes`] so all auth surfaces (`sender_auth`,
    /// `payer_auth`, `cfg.auth`, and per-actor authenticators) reject the reserved
    /// `< K1_AUTHENTICATOR` window identically. `address(0)` (the only address in
    /// that window) is the empty / "no actor configured" sentinel and is never a
    /// valid authenticator selector.
    pub fn authenticator_out_of_range(authenticator: &Address) -> bool {
        *authenticator < Eip8130Constants::K1_AUTHENTICATOR
    }

    /// Returns `true` when an authenticator selector may be used directly on the
    /// EIP-8130 transaction validation path: native k1 or a canonical Keystore
    /// authenticator. Before Zenith only native k1 reaches this: the pool and
    /// execution reject every other selector as unsupported first.
    pub fn authenticator_allowed_for_tx_path(authenticator: &Address) -> bool {
        *authenticator == Eip8130Constants::K1_AUTHENTICATOR
            || Eip8130Contracts::is_canonical_authenticator(authenticator)
    }

    /// Performs cheap selector-specific wire checks that do not require running
    /// an authenticator. Native k1 must carry exactly `r || s || v`; delegated
    /// auth must be depth-1 and name a canonical nested authenticator.
    pub fn authenticator_payload_well_formed(authenticator: &Address, data: &[u8]) -> bool {
        if *authenticator == Eip8130Constants::K1_AUTHENTICATOR {
            return data.len() == 65;
        }
        if *authenticator == Eip8130Contracts::DELEGATE_AUTHENTICATOR {
            if data.len() < 40 {
                return false;
            }
            let nested = Address::from_slice(&data[20..40]);
            return nested != Eip8130Contracts::DELEGATE_AUTHENTICATOR
                && Self::authenticator_allowed_for_tx_path(&nested);
        }
        true
    }

    /// Enforces the interim total-account-changes admission cap
    /// ([`Eip8130Constants::MAX_ACCOUNT_CHANGES_PER_TX`]) and then the per-entry
    /// structural invariants via [`Self::validate_account_change_entries`].
    ///
    /// The total cap is an interim pool-only throttle that currently sits below
    /// the per-type [`Eip8130Constants::MAX_CONFIG_CHANGES_PER_TX`] cap, so the
    /// per-type cap is exercised directly against
    /// [`Self::validate_account_change_entries`] in tests rather than through
    /// this gate.
    pub fn validate_account_changes(signed: &Eip8130Signed) -> Result<(), Eip8130StructuralError> {
        // Conservative admission cap on the number of account changes a single
        // transaction may carry while the interleaved authorize-and-apply flow
        // beds in. Keeps the per-transaction admission work (and the overlay it
        // applies against) small and bounded.
        if signed.tx().account_changes.len() > Eip8130Constants::MAX_ACCOUNT_CHANGES_PER_TX {
            return Err(Eip8130StructuralError::TooManyAccountChanges);
        }
        Self::validate_account_change_entries(signed)
    }

    /// Walks `account_changes` and enforces the per-entry structural invariants:
    /// at most one `Create` (and only as the first entry), at most one
    /// `Delegation`, `ConfigChange` count capped at
    /// [`Eip8130Constants::MAX_CONFIG_CHANGES_PER_TX`], and per-entry
    /// well-formedness. Chain binding is not checked here — it is enforced
    /// implicitly by the signed digest (`AccountChangeChannel` selects
    /// `block.chainid` vs `0`). Authenticator-address bounds are enforced on both
    /// `Create.initial_actors` and `ConfigChange.changes` via
    /// [`Self::validate_initial_actors`] and [`Self::validate_actor_changes`]
    /// respectively; actor-id *uniqueness* is required only for
    /// `Create.initial_actors` (strictly ascending), not for a signed change
    /// batch, whose ops the contract applies sequentially.
    ///
    /// This is the structural walk independent of the interim total cap applied
    /// by [`Self::validate_account_changes`], so the per-type caps it enforces
    /// remain meaningful (and testable) if that interim cap is later raised.
    pub fn validate_account_change_entries(
        signed: &Eip8130Signed,
    ) -> Result<(), Eip8130StructuralError> {
        let mut create_count = 0usize;
        let mut delegation_count = 0usize;
        let mut config_count = 0usize;
        for (idx, change) in signed.tx().account_changes.iter().enumerate() {
            match change {
                AccountChange::Create(create) => {
                    create_count += 1;
                    if create_count > 1 || idx != 0 {
                        return Err(Eip8130StructuralError::MalformedAccountChange);
                    }
                    // Reject at admission the runtime code shapes the enshrined
                    // deploy (`AccountChangeApplier::apply_create`) refuses:
                    // EIP-170 oversize and the EIP-3541 reserved leading `0xEF`
                    // byte (which `CREATE2` would reject with `address(0)`).
                    if create.code.is_empty()
                        || create.code.len() > Eip8130Constants::MAX_CODE_SIZE
                        || create.code.first() == Some(&0xEF)
                        || create.initial_actors.is_empty()
                    {
                        return Err(Eip8130StructuralError::MalformedAccountChange);
                    }
                    Self::validate_initial_actors(&create.initial_actors)?;
                }
                AccountChange::ConfigChange(cfg) => {
                    config_count += 1;
                    if config_count > Eip8130Constants::MAX_CONFIG_CHANGES_PER_TX {
                        return Err(Eip8130StructuralError::MalformedAccountChange);
                    }
                    // A signed batch must carry at least one op (mirrors the
                    // contract's `EmptyChangeSet` rejection).
                    if cfg.changes.is_empty() {
                        return Err(Eip8130StructuralError::MalformedAccountChange);
                    }
                    if cfg.signature.len() < 20 {
                        return Err(Eip8130StructuralError::MalformedAccountChange);
                    }
                    let cfg_authenticator = Address::from_slice(&cfg.signature[..20]);
                    if !Self::authenticator_allowed_for_tx_path(&cfg_authenticator)
                        || !Self::authenticator_payload_well_formed(
                            &cfg_authenticator,
                            &cfg.signature[20..],
                        )
                    {
                        return Err(Eip8130StructuralError::MalformedAccountChange);
                    }
                    Self::validate_actor_changes(&cfg.changes)?;
                }
                AccountChange::Delegation(_) => {
                    delegation_count += 1;
                    if delegation_count > 1 {
                        return Err(Eip8130StructuralError::MalformedAccountChange);
                    }
                    if create_count > 0 {
                        return Err(Eip8130StructuralError::MalformedAccountChange);
                    }
                }
            }
        }
        Ok(())
    }

    /// Validates `Create.initial_actors`: the slice length is bounded by
    /// [`Eip8130Constants::MAX_ACTORS_PER_ENTRY`] (anti-DoS cap on memory + work
    /// spent on duplicate detection), every `authenticator` is at or above the
    /// `K1_AUTHENTICATOR` floor (i.e. not the `address(0)` empty sentinel), no
    /// two entries share the same `actor_id`, and each entry's `policy_data` is
    /// a valid attachment length: empty, or exactly `manager (20) ||
    /// commitment (32)` (52 bytes). Length decides what gets stored; POLICY
    /// decides whether the sender is gated; OPERATOR overrides POLICY. The same
    /// length check is enforced downstream in `authorize_actor`/`slice_policy`;
    /// checking it here rejects malformed creates before the expensive overlay
    /// path runs.
    pub fn validate_initial_actors(actors: &[InitialActor]) -> Result<(), Eip8130StructuralError> {
        if actors.len() > Eip8130Constants::MAX_ACTORS_PER_ENTRY {
            return Err(Eip8130StructuralError::MalformedAccountChange);
        }
        let mut previous = None;
        for actor in actors {
            if Self::authenticator_out_of_range(&actor.authenticator) {
                return Err(Eip8130StructuralError::MalformedAccountChange);
            }
            if previous.is_some_and(|previous| actor.actor_id <= previous) {
                return Err(Eip8130StructuralError::MalformedAccountChange);
            }
            let len = actor.policy_data.len();
            if len != 0 && len != Eip8130Constants::POLICY_DATA_LEN {
                return Err(Eip8130StructuralError::MalformedAccountChange);
            }
            previous = Some(actor.actor_id);
        }
        Ok(())
    }

    /// Validates a signed batch's `changes`: the slice is bounded by
    /// [`Eip8130Constants::MAX_ACTOR_CHANGES_PER_CONFIG`], plus the
    /// reserved-window authenticator bound for the *new* actor of each
    /// `AuthorizeActor` op. Repeated `actorId` targets are *not* rejected here:
    /// unlike `Create.initial_actors`, the contract and the enshrined apply path
    /// process a batch's ops sequentially (authorize upserts, revoke clears), so
    /// a duplicate is protocol-valid (last write wins) and admitting it keeps the
    /// pool in step with consensus.
    ///
    /// - `AuthorizeActor`: `payload = abi.encode(bytes32 actorId, ActorConfig,
    ///   bytes)`; `ActorConfig.authenticator` is the right-aligned address in the
    ///   *second* word, so it is read from `payload[44..64]` (the leading 12
    ///   bytes of that word must be zero padding). Per EIP-8130 a config change
    ///   MAY authorize a non-canonical authenticator (for in-EVM use such as
    ///   recovery keys); only the reserved window (`< K1_AUTHENTICATOR`, i.e. the
    ///   `address(0)` empty sentinel) is rejected here.
    /// - `RevokeActor`: `payload = abi.encode(bytes32 actorId)` — exactly the
    ///   32-byte target and nothing more.
    /// - `IncrementLocalEpoch`: empty payload (mirrors the contract's
    ///   `payload.length == 0` requirement); it names no actor.
    /// - `Lock` / `Unlock`: their apply handlers are not yet enshrined, so a batch
    ///   carrying one is rejected here rather than admitted and failed later.
    pub fn validate_actor_changes(changes: &[SignedChange]) -> Result<(), Eip8130StructuralError> {
        if changes.len() > Eip8130Constants::MAX_ACTOR_CHANGES_PER_CONFIG {
            return Err(Eip8130StructuralError::MalformedAccountChange);
        }
        for change in changes {
            // Per-op structural well-formedness only. Repeated `actorId` targets
            // are intentionally NOT rejected: Keystore and the enshrined apply
            // path process a batch's ops in order (`AuthorizeActor` is an upsert,
            // `RevokeActor` clears), so a repeated target is valid on-chain (the
            // last write wins). Rejecting it here would drop a protocol-valid
            // batch, so the pool matches consensus and admits it.
            match change.change_type {
                ChangeType::AuthorizeActor => {
                    // `payload` = `abi.encode(bytes32 actorId, ActorConfig, bytes)`;
                    // the new actor's authenticator is the right-aligned address
                    // in the second word.
                    if change.payload.len() < 64 {
                        return Err(Eip8130StructuralError::MalformedAccountChange);
                    }
                    // The target `actorId` is `payload[0..32]`. `bytes32(0)` is the
                    // reserved "no actor" sentinel and can never be authorized;
                    // reject it up front to match `_authorizeActor`'s
                    // `InvalidActorId` (the enshrined apply path rejects it too).
                    if change.payload[..32].iter().all(|&b| b == 0) {
                        return Err(Eip8130StructuralError::MalformedAccountChange);
                    }
                    // The authenticator word is an ABI-encoded `address`: its
                    // leading 12 bytes are zero padding. Reject dirty upper bits so
                    // the gate and a strict ABI decoder downstream agree.
                    if change.payload[32..44].iter().any(|&b| b != 0) {
                        return Err(Eip8130StructuralError::MalformedAccountChange);
                    }
                    let authenticator = Address::from_slice(&change.payload[44..64]);
                    if Self::authenticator_out_of_range(&authenticator) {
                        return Err(Eip8130StructuralError::MalformedAccountChange);
                    }
                }
                ChangeType::RevokeActor => {
                    if change.payload.len() != 32 {
                        return Err(Eip8130StructuralError::MalformedAccountChange);
                    }
                }
                ChangeType::IncrementLocalEpoch => {
                    if !change.payload.is_empty() {
                        return Err(Eip8130StructuralError::MalformedAccountChange);
                    }
                    // Names no actor; skip the target-dedup.
                    continue;
                }
                ChangeType::Lock | ChangeType::Unlock => {
                    return Err(Eip8130StructuralError::MalformedAccountChange);
                }
            }
        }
        Ok(())
    }
}
