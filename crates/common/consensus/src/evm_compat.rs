//! EVM compatibility implementations for base-alloy consensus types.
//!
//! Provides [`FromRecoveredTx`] and [`FromTxWithEncoded`] impls for
//! [`OpTxEnvelope`] and [`TxDeposit`].

use alloy_eips::{Encodable2718, Typed2718};
use alloy_evm::{FromRecoveredTx, FromTxWithEncoded};
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use base_revm::{
    DepositTransactionParts, Eip8130AuthorizerValidation, Eip8130Call, Eip8130CodePlacement,
    Eip8130ConfigLog, Eip8130ConfigOp, Eip8130DelegateNativeValidation, Eip8130Parts,
    Eip8130SequenceUpdate, Eip8130StorageWrite, Eip8130VerifyCall, OpTransaction,
    custom_verifier_gas_cap,
};
use revm::context::TxEnv;

use crate::{
    ACCOUNT_CONFIG_ADDRESS, AccountChangeEntry, DELEGATE_VERIFIER_ADDRESS, K1_VERIFIER_ADDRESS,
    NONCE_KEY_MAX, OP_AUTHORIZE_OWNER, OP_REVOKE_OWNER, OpTxEnvelope, OwnerScope, TxDeposit,
    TxEip8130, VerifierGasCosts, account_change_units, auth_verifier_kind, auto_delegation_code,
    config_change_digest, config_change_sequence, config_change_writes, delegate_inner_verifier,
    derive_account_address, encode_verify_call, intrinsic_gas_with_costs,
    owner_registration_writes, payer_auth_cost, payer_signature_hash, payer_verification_gas,
    sender_signature_hash, total_verification_gas,
};
#[cfg(feature = "native-verifier")]
use crate::{
    NativeVerifier, NativeVerifyResult, ParsedSenderAuth, VerifierKind, parse_delegate_auth,
    parse_sender_auth, try_native_verify,
};

#[cfg(feature = "native-verifier")]
fn derive_sender_owner_id(tx: &TxEip8130) -> B256 {
    let parsed = match parse_sender_auth(tx) {
        Ok(parsed) => parsed,
        Err(_) => return B256::ZERO,
    };
    let sig_hash = sender_signature_hash(tx);

    match parsed {
        ParsedSenderAuth::Eoa { signature } => {
            let signature = Bytes::copy_from_slice(&signature);
            match try_native_verify(K1_VERIFIER_ADDRESS, &signature, sig_hash) {
                NativeVerifyResult::Verified(owner_id) => owner_id,
                NativeVerifyResult::Invalid(_) | NativeVerifyResult::Unsupported => B256::ZERO,
            }
        }
        ParsedSenderAuth::Configured { verifier, data } => {
            match NativeVerifier::from_address(verifier) {
                Some(native) => {
                    let mut delegate_owner = [0u8; 32];
                    if native == NativeVerifier::Delegate && data.len() >= 20 {
                        delegate_owner[..20].copy_from_slice(&data[..20]);
                    }
                    match try_native_verify(native.address(), &data, sig_hash) {
                        NativeVerifyResult::Verified(owner_id) => owner_id,
                        NativeVerifyResult::Invalid(_) => B256::ZERO,
                        NativeVerifyResult::Unsupported => {
                            if native == NativeVerifier::Delegate && data.len() >= 20 {
                                B256::from(delegate_owner)
                            } else {
                                B256::ZERO
                            }
                        }
                    }
                }
                None => B256::ZERO,
            }
        }
    }
}

#[cfg(not(feature = "native-verifier"))]
fn derive_sender_owner_id(_tx: &TxEip8130) -> B256 {
    B256::ZERO
}

/// Verifies the payer's signature and returns the payer `owner_id`.
///
/// For self-pay transactions returns `B256::ZERO`. For sponsored transactions,
/// the first 20 bytes of `payer_auth` identify the verifier address; the
/// remaining bytes are passed to the native verifier.
#[cfg(feature = "native-verifier")]
fn derive_payer_owner_id(tx: &TxEip8130) -> B256 {
    if tx.is_self_pay() {
        return B256::ZERO;
    }

    let Some(verifier) = auth_verifier_kind(&tx.payer_auth) else {
        return B256::ZERO;
    };
    let data = Bytes::copy_from_slice(&tx.payer_auth[20..]);
    let sig_hash = payer_signature_hash(tx);

    match verifier {
        VerifierKind::Native(native) => {
            let mut delegate_owner = [0u8; 32];
            if native == NativeVerifier::Delegate && data.len() >= 20 {
                delegate_owner[..20].copy_from_slice(&data[..20]);
            }
            match try_native_verify(native.address(), &data, sig_hash) {
                NativeVerifyResult::Verified(owner_id) => owner_id,
                NativeVerifyResult::Invalid(_) => B256::ZERO,
                NativeVerifyResult::Unsupported => {
                    if native == NativeVerifier::Delegate && data.len() >= 20 {
                        B256::from(delegate_owner)
                    } else {
                        B256::ZERO
                    }
                }
            }
        }
        VerifierKind::Custom(_) => B256::ZERO,
    }
}

#[cfg(not(feature = "native-verifier"))]
fn derive_payer_owner_id(_tx: &TxEip8130) -> B256 {
    B256::ZERO
}

#[cfg(feature = "native-verifier")]
fn derive_delegate_native_validation(
    auth: &[u8],
    sig_hash: B256,
) -> Option<Eip8130DelegateNativeValidation> {
    let parsed = parse_delegate_auth(auth)?;

    let delegate_data = Bytes::copy_from_slice(&auth[20..]);
    if !matches!(
        try_native_verify(DELEGATE_VERIFIER_ADDRESS, &delegate_data, sig_hash),
        NativeVerifyResult::Verified(_)
    ) {
        return None;
    }

    let delegate_account = parsed.delegate_account;
    let nested_verifier = parsed.nested_verifier;
    if nested_verifier == DELEGATE_VERIFIER_ADDRESS {
        return None;
    }

    let nested_data = Bytes::copy_from_slice(parsed.nested_data);
    let (verifier, owner_id) = if nested_verifier == Address::ZERO {
        let owner_id = match try_native_verify(K1_VERIFIER_ADDRESS, &nested_data, sig_hash) {
            NativeVerifyResult::Verified(owner_id) => owner_id,
            NativeVerifyResult::Invalid(_) | NativeVerifyResult::Unsupported => return None,
        };
        (K1_VERIFIER_ADDRESS, owner_id)
    } else if NativeVerifier::from_address(nested_verifier).is_some() {
        let owner_id = match try_native_verify(nested_verifier, &nested_data, sig_hash) {
            NativeVerifyResult::Verified(owner_id) => owner_id,
            NativeVerifyResult::Invalid(_) | NativeVerifyResult::Unsupported => return None,
        };
        (nested_verifier, owner_id)
    } else {
        return None;
    };

    Some(Eip8130DelegateNativeValidation { account: delegate_account, verifier, owner_id })
}

#[cfg(not(feature = "native-verifier"))]
fn derive_delegate_native_validation(
    _auth: &[u8],
    _sig_hash: B256,
) -> Option<Eip8130DelegateNativeValidation> {
    None
}

/// Builds a [`Eip8130VerifyCall`] for verifier auth that must run in-EVM.
///
/// Supported cases:
/// - **Custom verifier** auth: `CUSTOM(20) || data...`
/// - **Delegate auth with nested custom verifier**:
///   `DELEGATE(20) || delegate_account(20) || inner_auth(verifier(20) || data...)`
///
/// Returns `None` for native-only paths that do not require runtime STATICCALL.
fn build_verify_call(
    auth: &[u8],
    sig_hash: B256,
    account: Address,
    required_scope: u8,
    direct_nested_custom_delegate: bool,
) -> Option<Eip8130VerifyCall> {
    let verifier_kind = auth_verifier_kind(auth)?;
    let verifier = verifier_kind.address();

    if verifier_kind.is_custom() {
        let data = Bytes::copy_from_slice(&auth[20..]);
        let calldata = encode_verify_call(sig_hash, &data);
        return Some(Eip8130VerifyCall { verifier, calldata, account, required_scope });
    }

    // Delegate envelope:
    // DELEGATE(20) || delegate_account(20) || nested_auth(verifier(20) || ...)
    //
    // Nested native verifiers stay fully native. Nested custom verifiers can
    // either route directly to the nested verifier (native delegate wrapper) or
    // through the Solidity delegate contract, depending on caller policy.
    if verifier == DELEGATE_VERIFIER_ADDRESS && auth.len() >= 60 {
        let inner_verifier = Address::from_slice(&auth[40..60]);
        if inner_verifier != Address::ZERO
            && auth_verifier_kind(&auth[40..]).is_some_and(|inner| inner.is_custom())
        {
            let (call_verifier, call_account, data) = if direct_nested_custom_delegate {
                (
                    inner_verifier,
                    Address::from_slice(&auth[20..40]),
                    Bytes::copy_from_slice(&auth[60..]),
                )
            } else {
                (verifier, account, Bytes::copy_from_slice(&auth[20..]))
            };
            let calldata = encode_verify_call(sig_hash, &data);
            return Some(Eip8130VerifyCall {
                verifier: call_verifier,
                calldata,
                account: call_account,
                required_scope,
            });
        }
    }

    None
}

/// Builds per-config-change authorizer validation data.
///
/// For each `ConfigChangeEntry`, computes the config change digest, then:
/// - **Custom verifier (non-native address):** builds an [`Eip8130VerifyCall`] for runtime
///   STATICCALL.
/// - **Native verifier:** runs `try_native_verify` to obtain the `owner_id`.
///
/// Returns one [`Eip8130AuthorizerValidation`] per config change entry.
#[cfg(feature = "native-verifier")]
fn build_authorizer_validations(
    tx: &TxEip8130,
    sender: Address,
) -> Vec<Eip8130AuthorizerValidation> {
    let mut validations = Vec::new();
    for entry in &tx.account_changes {
        let cc = match entry {
            AccountChangeEntry::ConfigChange(cc) => cc,
            _ => continue,
        };
        if cc.authorizer_auth.len() < 20 {
            validations.push(Eip8130AuthorizerValidation::default());
            continue;
        }

        let digest = config_change_digest(sender, cc);
        let verifier = auth_verifier_kind(&cc.authorizer_auth).expect("len checked above");
        let verifier_address = verifier.address();

        let ops: Vec<Eip8130ConfigOp> = cc
            .owner_changes
            .iter()
            .map(|op| Eip8130ConfigOp {
                change_type: op.change_type,
                verifier: op.verifier,
                owner_id: op.owner_id,
                scope: op.scope,
            })
            .collect();

        if verifier.is_custom() {
            let verify_call =
                build_verify_call(&cc.authorizer_auth, digest, sender, OwnerScope::CONFIG, false);
            validations.push(Eip8130AuthorizerValidation {
                verifier: verifier_address,
                owner_id: B256::ZERO,
                verify_call,
                owner_changes: ops,
            });
        } else {
            let data = Bytes::copy_from_slice(&cc.authorizer_auth[20..]);
            let owner_id = match try_native_verify(verifier_address, &data, digest) {
                NativeVerifyResult::Verified(id) => id,
                NativeVerifyResult::Invalid(_) | NativeVerifyResult::Unsupported => B256::ZERO,
            };
            validations.push(Eip8130AuthorizerValidation {
                verifier: verifier_address,
                owner_id,
                verify_call: None,
                owner_changes: ops,
            });
        }
    }
    validations
}

#[cfg(not(feature = "native-verifier"))]
fn build_authorizer_validations(
    tx: &TxEip8130,
    sender: Address,
) -> Vec<Eip8130AuthorizerValidation> {
    let mut validations = Vec::new();
    for entry in &tx.account_changes {
        let cc = match entry {
            AccountChangeEntry::ConfigChange(cc) => cc,
            _ => continue,
        };
        if cc.authorizer_auth.len() < 20 {
            validations.push(Eip8130AuthorizerValidation::default());
            continue;
        }

        let digest = config_change_digest(sender, cc);
        let verifier = auth_verifier_kind(&cc.authorizer_auth).expect("len checked above");
        let verifier_address = verifier.address();

        let ops: Vec<Eip8130ConfigOp> = cc
            .owner_changes
            .iter()
            .map(|op| Eip8130ConfigOp {
                change_type: op.change_type,
                verifier: op.verifier,
                owner_id: op.owner_id,
                scope: op.scope,
            })
            .collect();

        let verify_call = if verifier.is_custom() {
            build_verify_call(&cc.authorizer_auth, digest, sender, OwnerScope::CONFIG, false)
        } else {
            None
        };

        validations.push(Eip8130AuthorizerValidation {
            verifier: verifier_address,
            owner_id: B256::ZERO,
            verify_call,
            owner_changes: ops,
        });
    }
    validations
}

/// Build [`Eip8130Parts`] from a decoded [`TxEip8130`] for use by the handler.
///
/// `recovered_caller` is the ecrecovered sender address. For EOA mode
/// (`from` empty) this is the address derived from ecrecover;
/// for configured mode it equals `tx.from`.
///
/// Uses [`VerifierGasCosts::BASE_V1`] for verification gas computation.
/// Call [`build_eip8130_parts_with_costs`] to supply custom gas costs.
pub fn build_eip8130_parts(tx: &TxEip8130, recovered_caller: Address) -> Eip8130Parts {
    build_eip8130_parts_with_costs(tx, recovered_caller, &VerifierGasCosts::BASE_V1)
}

/// Build [`Eip8130Parts`] from a decoded [`TxEip8130`] with explicit gas costs.
///
/// `recovered_caller` is the ecrecovered sender address. For EOA mode
/// it overrides an empty `tx.from`. For self-pay
/// transactions, the payer is also set to `recovered_caller`.
pub fn build_eip8130_parts_with_costs(
    tx: &TxEip8130,
    recovered_caller: Address,
    costs: &VerifierGasCosts,
) -> Eip8130Parts {
    let sender = recovered_caller;
    let payer = tx.payer.unwrap_or(recovered_caller);
    let owner_id = derive_sender_owner_id(tx);
    let payer_owner_id = derive_payer_owner_id(tx);

    let sender_inner = delegate_inner_verifier(&tx.sender_auth);
    let payer_inner = delegate_inner_verifier(&tx.payer_auth);
    let verification_gas = total_verification_gas(tx, costs, sender_inner, payer_inner);

    let has_create_entry =
        tx.account_changes.iter().any(|e| matches!(e, AccountChangeEntry::Create(_)));
    let total_account_change_units = account_change_units(tx);
    let total_config_ops = tx
        .account_changes
        .iter()
        .filter_map(|entry| match entry {
            AccountChangeEntry::ConfigChange(cc) => Some(cc.owner_changes.len()),
            _ => None,
        })
        .sum();

    let mut pre_writes = Vec::new();
    let mut config_writes = Vec::new();
    let mut sequence_updates = Vec::new();
    let mut code_placements = Vec::new();
    let mut account_creation_logs = Vec::new();
    let mut config_change_logs = Vec::new();
    for entry in &tx.account_changes {
        match entry {
            AccountChangeEntry::Create(create) => {
                let account = derive_account_address(
                    ACCOUNT_CONFIG_ADDRESS,
                    create.user_salt,
                    &create.bytecode,
                    &create.initial_owners,
                );
                for w in owner_registration_writes(account, create) {
                    pre_writes.push(Eip8130StorageWrite {
                        address: w.address,
                        slot: w.slot,
                        value: w.value,
                    });
                }
                code_placements
                    .push(Eip8130CodePlacement { address: account, code: create.bytecode.clone() });

                for owner in &create.initial_owners {
                    account_creation_logs.push(Eip8130ConfigLog::OwnerAuthorized {
                        account,
                        owner_id: owner.owner_id,
                        verifier: owner.verifier,
                        scope: owner.scope,
                    });
                }
                account_creation_logs.push(Eip8130ConfigLog::AccountCreated {
                    account,
                    user_salt: create.user_salt,
                    code_hash: keccak256(&create.bytecode),
                });
            }
            AccountChangeEntry::ConfigChange(cc) => {
                for w in config_change_writes(sender, cc) {
                    config_writes.push(Eip8130StorageWrite {
                        address: w.address,
                        slot: w.slot,
                        value: w.value,
                    });
                }
                let seq = config_change_sequence(sender, cc);
                sequence_updates.push(Eip8130SequenceUpdate {
                    slot: seq.slot,
                    is_multichain: seq.is_multichain,
                    new_value: seq.new_value,
                });

                for op in &cc.owner_changes {
                    match op.change_type {
                        OP_AUTHORIZE_OWNER => {
                            config_change_logs.push(Eip8130ConfigLog::OwnerAuthorized {
                                account: sender,
                                owner_id: op.owner_id,
                                verifier: op.verifier,
                                scope: op.scope,
                            });
                        }
                        OP_REVOKE_OWNER => {
                            config_change_logs.push(Eip8130ConfigLog::OwnerRevoked {
                                account: sender,
                                owner_id: op.owner_id,
                            });
                        }
                        _ => {}
                    }
                }
            }
            AccountChangeEntry::Delegation(_) => {
                // delegation_target is extracted below
            }
        }
    }

    let call_phases: Vec<Vec<Eip8130Call>> = tx
        .calls
        .iter()
        .map(|phase| {
            phase
                .iter()
                .map(|call| Eip8130Call { to: call.to, data: call.data.clone(), value: U256::ZERO })
                .collect()
        })
        .collect();

    let sender_verify_call = if !tx.is_eoa() {
        let sig_hash = sender_signature_hash(tx);
        build_verify_call(&tx.sender_auth, sig_hash, sender, OwnerScope::SENDER, true)
    } else {
        None
    };
    let sender_delegate_native_validation = if !tx.is_eoa() {
        let sig_hash = sender_signature_hash(tx);
        derive_delegate_native_validation(&tx.sender_auth, sig_hash)
    } else {
        None
    };

    let payer_verify_call = if !tx.is_self_pay() && !tx.payer_auth.is_empty() {
        let sig_hash = payer_signature_hash(tx);
        build_verify_call(&tx.payer_auth, sig_hash, payer, OwnerScope::PAYER, true)
    } else {
        None
    };
    let payer_delegate_native_validation = if !tx.is_self_pay() && !tx.payer_auth.is_empty() {
        let sig_hash = payer_signature_hash(tx);
        derive_delegate_native_validation(&tx.payer_auth, sig_hash)
    } else {
        None
    };

    let aa_intrinsic_gas = intrinsic_gas_with_costs(
        tx,
        false, /* cold nonce — worst case */
        tx.chain_id,
        costs,
    );

    let sender_auth_empty = !tx.is_eoa() && tx.sender_auth.is_empty();
    let payer_auth_empty = !tx.is_self_pay() && tx.payer_auth.is_empty();

    let payer_intrinsic_gas = payer_auth_cost(tx) + payer_verification_gas(tx, costs, payer_inner);

    let authorizer_validations = build_authorizer_validations(tx, sender);

    let nonce_free_hash =
        if tx.nonce_key == NONCE_KEY_MAX { Some(sender_signature_hash(tx)) } else { None };

    let delegation_target = tx.account_changes.iter().find_map(|e| match e {
        AccountChangeEntry::Delegation(d) => Some(d.target),
        _ => None,
    });

    let has_custom_verifier = tx.has_custom_verifier();
    let custom_verifier_gas_cap = if has_custom_verifier { custom_verifier_gas_cap() } else { 0 };

    let sender_verifier = if tx.is_eoa() {
        K1_VERIFIER_ADDRESS
    } else {
        auth_verifier_kind(&tx.sender_auth).map_or(Address::ZERO, |kind| kind.address())
    };

    let payer_verifier = if tx.is_self_pay() {
        Address::ZERO
    } else {
        auth_verifier_kind(&tx.payer_auth).map_or(Address::ZERO, |kind| kind.address())
    };

    Eip8130Parts {
        expiry: tx.expiry,
        sender,
        payer,
        owner_id,
        payer_owner_id,
        nonce_key: tx.nonce_key,
        nonce_free_hash,
        has_create_entry,
        delegation_target,
        account_change_units: total_account_change_units,
        total_config_ops,
        verification_gas,
        aa_intrinsic_gas,
        payer_intrinsic_gas,
        custom_verifier_gas_cap,
        sender_verifier,
        payer_verifier,
        auto_delegation_code: auto_delegation_code(),
        pre_writes,
        config_writes,
        sequence_updates,
        code_placements,
        call_phases,
        sender_verify_call,
        payer_verify_call,
        sender_delegate_native_validation,
        payer_delegate_native_validation,
        authorizer_validations,
        account_creation_logs,
        config_change_logs,
        sender_auth_empty,
        payer_auth_empty,
    }
}

// ---------------------------------------------------------------------------
// FromRecoveredTx / FromTxWithEncoded – OpTxEnvelope -> TxEnv
// ---------------------------------------------------------------------------

impl FromRecoveredTx<OpTxEnvelope> for TxEnv {
    fn from_recovered_tx(tx: &OpTxEnvelope, caller: Address) -> Self {
        match tx {
            OpTxEnvelope::Legacy(tx) => Self::from_recovered_tx(tx.tx(), caller),
            OpTxEnvelope::Eip1559(tx) => Self::from_recovered_tx(tx.tx(), caller),
            OpTxEnvelope::Eip2930(tx) => Self::from_recovered_tx(tx.tx(), caller),
            OpTxEnvelope::Eip7702(tx) => Self::from_recovered_tx(tx.tx(), caller),
            OpTxEnvelope::Eip8130(tx) => {
                let inner = tx.inner();
                // NOTE: authorization_list is intentionally not copied to TxEnv for
                // AA (type 0x7B) transactions. The AA handler applies auto-delegation
                // directly and does not invoke revm's standard EIP-7702 processing.
                //
                // `gas_limit` in TxEip8130 is execution-only. Revm expects the
                // total gas limit (intrinsic + verification + execution). We add
                // intrinsic gas (cold nonce worst-case) and the custom verifier gas
                // cap. The handler's `validate_initial_tx_gas` deducts intrinsic;
                // verification gets its own cap separate from execution budget.
                let aa_intrinsic = intrinsic_gas_with_costs(
                    inner,
                    false, // cold nonce — worst case
                    inner.chain_id,
                    &VerifierGasCosts::BASE_V1,
                );
                let has_custom = inner.has_custom_verifier();
                let verifier_cap = if has_custom { custom_verifier_gas_cap() } else { 0 };
                Self {
                    tx_type: inner.ty(),
                    caller,
                    gas_limit: aa_intrinsic
                        .saturating_add(verifier_cap)
                        .saturating_add(inner.gas_limit),
                    nonce: inner.nonce_sequence,
                    chain_id: Some(inner.chain_id),
                    kind: revm::primitives::TxKind::Call(caller),
                    value: alloy_primitives::U256::ZERO,
                    data: alloy_primitives::Bytes::default(),
                    gas_price: inner.max_fee_per_gas,
                    gas_priority_fee: Some(inner.max_priority_fee_per_gas),
                    ..Default::default()
                }
            }
            OpTxEnvelope::Deposit(tx) => Self::from_recovered_tx(tx.inner(), caller),
        }
    }
}

impl FromRecoveredTx<TxDeposit> for TxEnv {
    fn from_recovered_tx(tx: &TxDeposit, caller: Address) -> Self {
        let TxDeposit {
            to,
            value,
            gas_limit,
            input,
            source_hash: _,
            from: _,
            mint: _,
            is_system_transaction: _,
        } = tx;
        Self {
            tx_type: tx.ty(),
            caller,
            gas_limit: *gas_limit,
            kind: *to,
            value: *value,
            data: input.clone(),
            ..Default::default()
        }
    }
}

impl FromTxWithEncoded<OpTxEnvelope> for TxEnv {
    fn from_encoded_tx(tx: &OpTxEnvelope, caller: Address, _encoded: Bytes) -> Self {
        Self::from_recovered_tx(tx, caller)
    }
}

// ---------------------------------------------------------------------------
// FromRecoveredTx / FromTxWithEncoded – OpTxEnvelope -> OpTransaction<TxEnv>
// ---------------------------------------------------------------------------

impl FromRecoveredTx<OpTxEnvelope> for OpTransaction<TxEnv> {
    fn from_recovered_tx(tx: &OpTxEnvelope, sender: Address) -> Self {
        let encoded = tx.encoded_2718();
        Self::from_encoded_tx(tx, sender, encoded.into())
    }
}

impl FromTxWithEncoded<OpTxEnvelope> for OpTransaction<TxEnv> {
    fn from_encoded_tx(tx: &OpTxEnvelope, caller: Address, encoded: Bytes) -> Self {
        match tx {
            OpTxEnvelope::Legacy(tx) => Self {
                base: TxEnv::from_recovered_tx(tx.tx(), caller),
                enveloped_tx: Some(encoded),
                deposit: Default::default(),
                eip8130: Default::default(),
            },
            OpTxEnvelope::Eip1559(tx) => Self {
                base: TxEnv::from_recovered_tx(tx.tx(), caller),
                enveloped_tx: Some(encoded),
                deposit: Default::default(),
                eip8130: Default::default(),
            },
            OpTxEnvelope::Eip2930(tx) => Self {
                base: TxEnv::from_recovered_tx(tx.tx(), caller),
                enveloped_tx: Some(encoded),
                deposit: Default::default(),
                eip8130: Default::default(),
            },
            OpTxEnvelope::Eip7702(tx) => Self {
                base: TxEnv::from_recovered_tx(tx.tx(), caller),
                enveloped_tx: Some(encoded),
                deposit: Default::default(),
                eip8130: Default::default(),
            },
            OpTxEnvelope::Eip8130(tx) => {
                let eip8130 = build_eip8130_parts(tx.inner(), caller);
                Self {
                    base: TxEnv::from_recovered_tx(&OpTxEnvelope::Eip8130(tx.clone()), caller),
                    enveloped_tx: Some(encoded),
                    deposit: Default::default(),
                    eip8130,
                }
            }
            OpTxEnvelope::Deposit(tx) => Self::from_encoded_tx(tx.inner(), caller, encoded),
        }
    }
}

// ---------------------------------------------------------------------------
// TxDeposit -> OpTransaction<TxEnv>
// ---------------------------------------------------------------------------

impl FromRecoveredTx<TxDeposit> for OpTransaction<TxEnv> {
    fn from_recovered_tx(tx: &TxDeposit, sender: Address) -> Self {
        let encoded = tx.encoded_2718();
        Self::from_encoded_tx(tx, sender, encoded.into())
    }
}

impl FromTxWithEncoded<TxDeposit> for OpTransaction<TxEnv> {
    fn from_encoded_tx(tx: &TxDeposit, caller: Address, encoded: Bytes) -> Self {
        let base = TxEnv::from_recovered_tx(tx, caller);
        let deposit = DepositTransactionParts {
            source_hash: tx.source_hash,
            mint: Some(tx.mint),
            is_system_transaction: tx.is_system_transaction,
        };
        Self { base, enveloped_tx: Some(encoded), deposit, eip8130: Default::default() }
    }
}

#[cfg(all(test, feature = "native-verifier"))]
mod tests {
    use alloy_primitives::keccak256;
    use k256::{
        ecdsa::{SigningKey, signature::hazmat::PrehashSigner},
        elliptic_curve::rand_core::OsRng,
    };

    use super::*;

    fn address_from_signing_key(signing_key: &SigningKey) -> Address {
        let pubkey = signing_key.verifying_key().to_encoded_point(false);
        let hash = keccak256(&pubkey.as_bytes()[1..]);
        Address::from_slice(&hash[12..])
    }

    #[test]
    fn derive_owner_id_for_configured_k1_sender() {
        let signing_key = SigningKey::random(&mut OsRng);
        let sender = address_from_signing_key(&signing_key);

        let mut tx = TxEip8130 {
            chain_id: 8453,
            from: Some(sender),
            nonce_key: U256::ZERO,
            nonce_sequence: 7,
            max_priority_fee_per_gas: 1,
            max_fee_per_gas: 1,
            gas_limit: 21_000,
            ..Default::default()
        };

        let sig_hash = sender_signature_hash(&tx);
        let (signature, recovery_id) = signing_key.sign_prehash(sig_hash.as_slice()).unwrap();
        let mut auth = Vec::with_capacity(85);
        auth.extend_from_slice(K1_VERIFIER_ADDRESS.as_slice());
        auth.extend_from_slice(&signature.to_bytes());
        auth.push(recovery_id.to_byte());
        tx.sender_auth = Bytes::from(auth);

        let owner_id = derive_sender_owner_id(&tx);
        let mut expected = [0u8; 32];
        expected[..20].copy_from_slice(sender.as_slice());
        assert_eq!(owner_id, B256::from(expected));
    }

    #[test]
    fn derive_owner_id_unsupported_verifier_returns_zero() {
        let tx = TxEip8130 {
            from: Some(Address::repeat_byte(0x11)),
            sender_auth: Bytes::copy_from_slice(Address::repeat_byte(0x22).as_slice()),
            ..Default::default()
        };

        assert_eq!(derive_sender_owner_id(&tx), B256::ZERO);
    }

    #[test]
    fn build_parts_populates_delegate_native_validation() {
        let signing_key = SigningKey::random(&mut OsRng);
        let sender = Address::repeat_byte(0x11);
        let delegate_account = Address::repeat_byte(0x22);

        let mut tx = TxEip8130 {
            chain_id: 8453,
            from: Some(sender),
            nonce_key: U256::ZERO,
            nonce_sequence: 7,
            max_priority_fee_per_gas: 1,
            max_fee_per_gas: 1,
            gas_limit: 21_000,
            ..Default::default()
        };
        let sig_hash = sender_signature_hash(&tx);
        let (signature, recovery_id) = signing_key.sign_prehash(sig_hash.as_slice()).unwrap();

        let mut sender_auth = Vec::new();
        sender_auth.extend_from_slice(DELEGATE_VERIFIER_ADDRESS.as_slice());
        sender_auth.extend_from_slice(delegate_account.as_slice());
        sender_auth.extend_from_slice(K1_VERIFIER_ADDRESS.as_slice());
        sender_auth.extend_from_slice(&signature.to_bytes());
        sender_auth.push(recovery_id.to_byte());
        tx.sender_auth = Bytes::from(sender_auth);

        let parts = build_eip8130_parts_with_costs(&tx, sender, &VerifierGasCosts::BASE_V1);
        let delegate = parts
            .sender_delegate_native_validation
            .expect("delegate metadata should be populated for native nested verifier");
        assert_eq!(delegate.account, delegate_account);
        assert_eq!(delegate.verifier, K1_VERIFIER_ADDRESS);
        assert_ne!(delegate.owner_id, B256::ZERO);
    }

    #[test]
    fn build_eip8130_parts_preserves_native_authorizer_verifier() {
        let sender = Address::repeat_byte(0x11);
        let mut tx = TxEip8130 {
            chain_id: 8453,
            from: Some(sender),
            nonce_key: U256::ZERO,
            nonce_sequence: 1,
            max_priority_fee_per_gas: 1,
            max_fee_per_gas: 1,
            gas_limit: 21_000,
            ..Default::default()
        };

        // Keep payload short/invalid on purpose; we only care that conversion
        // preserves which verifier was specified in authorizer_auth.
        let mut auth = K1_VERIFIER_ADDRESS.as_slice().to_vec();
        auth.extend_from_slice(&[0u8; 65]);
        tx.account_changes = vec![AccountChangeEntry::ConfigChange(crate::ConfigChangeEntry {
            chain_id: 8453,
            sequence: 0,
            owner_changes: Vec::new(),
            authorizer_auth: Bytes::from(auth),
        })];

        let parts = build_eip8130_parts_with_costs(&tx, sender, &VerifierGasCosts::BASE_V1);
        assert_eq!(parts.authorizer_validations.len(), 1);
        let validation = &parts.authorizer_validations[0];
        assert_eq!(validation.verifier, K1_VERIFIER_ADDRESS);
        assert!(validation.verify_call.is_none());
    }

    #[test]
    fn build_eip8130_parts_preserves_custom_authorizer_verifier() {
        let sender = Address::repeat_byte(0x11);
        let custom_verifier = Address::repeat_byte(0x77);
        let mut tx = TxEip8130 {
            chain_id: 8453,
            from: Some(sender),
            nonce_key: U256::ZERO,
            nonce_sequence: 1,
            max_priority_fee_per_gas: 1,
            max_fee_per_gas: 1,
            gas_limit: 21_000,
            ..Default::default()
        };

        let mut auth = custom_verifier.as_slice().to_vec();
        auth.push(0x01);
        tx.account_changes = vec![AccountChangeEntry::ConfigChange(crate::ConfigChangeEntry {
            chain_id: 8453,
            sequence: 0,
            owner_changes: Vec::new(),
            authorizer_auth: Bytes::from(auth),
        })];

        let parts = build_eip8130_parts_with_costs(&tx, sender, &VerifierGasCosts::BASE_V1);
        assert_eq!(parts.authorizer_validations.len(), 1);
        let validation = &parts.authorizer_validations[0];
        assert_eq!(validation.verifier, custom_verifier);
        assert!(validation.verify_call.is_some());
    }
}
