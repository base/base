//! EIP-8130 intrinsic gas calculation.

use alloy_primitives::Address;
use alloy_rlp::Encodable;

use super::{
    AccountChangeEntry, TxEip8130,
    constants::{
        AA_BASE_COST, BYTECODE_BASE_GAS, BYTECODE_PER_BYTE_GAS, CONFIG_CHANGE_OP_GAS,
        CONFIG_CHANGE_SKIP_GAS, EXPIRING_NONCE_GAS, NONCE_KEY_COLD_GAS, NONCE_KEY_MAX,
        NONCE_KEY_WARM_GAS, SLOAD_GAS, VerifierGasCosts,
    },
    delegate_inner_verifier,
    predeploys::K1_VERIFIER_ADDRESS,
};

/// Computes the intrinsic gas for an AA transaction.
///
/// ```text
/// intrinsic_gas = AA_BASE_COST
///               + tx_payload_cost
///               + sender_auth_cost
///               + payer_auth_cost
///               + verification_gas  (sender + payer native verifier costs)
///               + nonce_key_cost
///               + bytecode_cost
///               + account_changes_cost
/// ```
///
/// Intrinsic gas is protocol-computed and non-refundable. The sender's
/// `gas_limit` is execution-only (calls); intrinsic gas is charged on top.
/// Total charge to payer: `effective_gas_price * (intrinsic_gas + execution_gas_used)`.
///
/// **Custom verifiers** (non-native addresses) contribute 0 to
/// `verification_gas` here because their cost is determined at runtime via
/// STATICCALL, capped by the node's configured custom-verifier gas budget, and
/// charged to the payer separately.
///
/// The `nonce_key_is_warm` parameter indicates whether the nonce channel has been
/// used before (affects the SSTORE cost). Verification gas uses the default
/// [`VerifierGasCosts::BASE_V1`] schedule. Delegate inner verifiers are
/// automatically extracted from the auth blobs.
pub fn intrinsic_gas(tx: &TxEip8130, nonce_key_is_warm: bool, chain_id: u64) -> u64 {
    intrinsic_gas_with_costs(tx, nonce_key_is_warm, chain_id, &VerifierGasCosts::BASE_V1)
}

/// Computes intrinsic gas with explicit verifier gas costs.
pub fn intrinsic_gas_with_costs(
    tx: &TxEip8130,
    nonce_key_is_warm: bool,
    chain_id: u64,
    costs: &VerifierGasCosts,
) -> u64 {
    let sender_inner = delegate_inner_verifier(&tx.sender_auth);
    let payer_inner = delegate_inner_verifier(&tx.payer_auth);

    let mut gas = AA_BASE_COST;

    gas += tx_payload_cost(tx);
    gas += sender_auth_cost(tx);
    gas += payer_auth_cost(tx);
    gas += total_verification_gas(tx, costs, sender_inner, payer_inner);
    gas += authorizer_verification_gas(tx, costs);
    if tx.nonce_key != NONCE_KEY_MAX {
        gas += nonce_key_cost(nonce_key_is_warm);
    } else {
        gas += EXPIRING_NONCE_GAS;
    }
    gas += bytecode_cost(tx);
    gas += account_changes_cost(tx, chain_id);

    gas
}

/// Standard EIP-2028 calldata cost: 16 gas per non-zero byte, 4 per zero byte,
/// computed over the full RLP encoding of the transaction.
pub fn tx_payload_cost(tx: &TxEip8130) -> u64 {
    let mut buf = alloc::vec::Vec::with_capacity(tx.length());
    tx.encode(&mut buf);
    calldata_gas(&buf)
}

/// Sender authentication overhead (SLOAD for owner_config read).
///
/// Always charges an SLOAD (2 100) to read the owner config slot, even for
/// EOA-mode accounts — the config must be checked to verify the key has
/// not been revoked and the account is not locked.
///
/// The actual cryptographic verification cost is charged separately via
/// [`sender_verification_gas`].
pub fn sender_auth_cost(_tx: &TxEip8130) -> u64 {
    SLOAD_GAS
}

/// Payer authentication cost (SLOAD overhead, excluding verification gas): 0 for self-pay.
pub fn payer_auth_cost(tx: &TxEip8130) -> u64 {
    if tx.is_self_pay() { 0 } else { SLOAD_GAS }
}

/// Gas charged for native cryptographic verification of the sender's signature.
///
/// For EOA mode, the verifier is implicitly K1. For configured owners,
/// the first 20 bytes of `sender_auth` identify the verifier address.
///
/// When `sender_auth` is empty (e.g. during `eth_estimateGas` with dummy auth),
/// defaults to K1 gas so that gas estimates are not underestimated.
///
/// The `inner_verifier` is only used for DELEGATE; the caller must
/// resolve the delegation target and provide the inner verifier address.
pub fn sender_verification_gas(
    tx: &TxEip8130,
    costs: &VerifierGasCosts,
    inner_verifier: Option<Address>,
) -> u64 {
    if tx.is_eoa() || tx.sender_auth.is_empty() {
        costs.gas_for_verifier(K1_VERIFIER_ADDRESS, None)
    } else {
        let verifier = Address::from_slice(&tx.sender_auth[..20]);
        costs.gas_for_verifier(verifier, inner_verifier)
    }
}

/// Gas charged for native cryptographic verification of the payer's signature.
///
/// Returns 0 for self-pay transactions. When `payer_auth` is empty on a
/// sponsored transaction (e.g. during `eth_estimateGas`), defaults to K1 gas.
pub fn payer_verification_gas(
    tx: &TxEip8130,
    costs: &VerifierGasCosts,
    inner_verifier: Option<Address>,
) -> u64 {
    if tx.is_self_pay() {
        return 0;
    }
    if tx.payer_auth.is_empty() {
        return costs.gas_for_verifier(K1_VERIFIER_ADDRESS, None);
    }
    let verifier = Address::from_slice(&tx.payer_auth[..20]);
    costs.gas_for_verifier(verifier, inner_verifier)
}

/// Total verification gas for both sender and payer.
pub fn total_verification_gas(
    tx: &TxEip8130,
    costs: &VerifierGasCosts,
    sender_inner: Option<Address>,
    payer_inner: Option<Address>,
) -> u64 {
    sender_verification_gas(tx, costs, sender_inner)
        + payer_verification_gas(tx, costs, payer_inner)
}

/// Gas charged for config change authorizer verification.
///
/// Each `ConfigChangeEntry` has an `authorizer_auth` blob signed by an owner
/// with CONFIG scope. For native verifiers the gas is included in intrinsic;
/// for custom verifiers (non-native addresses) it returns 0 (metered at
/// runtime via the shared custom-verifier gas budget).
///
/// Also charges an SLOAD per config change for the owner_config read.
pub fn authorizer_verification_gas(tx: &TxEip8130, costs: &VerifierGasCosts) -> u64 {
    let mut gas = 0u64;
    for entry in &tx.account_changes {
        if let AccountChangeEntry::ConfigChange(cc) = entry {
            if cc.authorizer_auth.is_empty() {
                continue;
            }
            let verifier = Address::from_slice(&cc.authorizer_auth[..20]);
            let inner = delegate_inner_verifier(&cc.authorizer_auth);
            gas += costs.gas_for_verifier(verifier, inner);
            gas += SLOAD_GAS; // owner_config read for CONFIG scope check
        }
    }
    gas
}

/// Nonce key cost: 22 100 for a new channel, 5 000 for an existing one.
pub fn nonce_key_cost(is_warm: bool) -> u64 {
    if is_warm { NONCE_KEY_WARM_GAS } else { NONCE_KEY_COLD_GAS }
}

/// Bytecode deployment cost: 32 000 base + 200/byte if a create entry is present.
pub fn bytecode_cost(tx: &TxEip8130) -> u64 {
    for entry in &tx.account_changes {
        if let AccountChangeEntry::Create(create) = entry {
            if create.bytecode.is_empty() {
                return BYTECODE_BASE_GAS;
            }
            return BYTECODE_BASE_GAS + BYTECODE_PER_BYTE_GAS * create.bytecode.len() as u64;
        }
    }
    0
}

/// Configuration change cost.
///
/// - Create entry: `CONFIG_CHANGE_OP_GAS * (1 + initial_owners.len())`
/// - Config entries targeting the current `chain_id` (or chain_id == 0):
///   `CONFIG_CHANGE_OP_GAS` per operation.
/// - Entries for a different chain: `CONFIG_CHANGE_SKIP_GAS` (SLOAD to verify and skip).
pub fn account_changes_cost(tx: &TxEip8130, chain_id: u64) -> u64 {
    let mut gas = 0u64;
    for entry in &tx.account_changes {
        match entry {
            AccountChangeEntry::Create(create) => {
                gas += CONFIG_CHANGE_OP_GAS * (1 + create.initial_owners.len() as u64);
            }
            AccountChangeEntry::ConfigChange(cc) => {
                if cc.chain_id == 0 || cc.chain_id == chain_id {
                    gas += CONFIG_CHANGE_OP_GAS * cc.owner_changes.len() as u64;
                } else {
                    gas += CONFIG_CHANGE_SKIP_GAS;
                }
            }
            AccountChangeEntry::Delegation(_) => {
                gas += super::constants::BYTECODE_PER_BYTE_GAS * 23;
            }
        }
    }
    gas
}

/// Counts account-change units in a transaction.
///
/// Counting rules:
/// - each create entry counts as 1,
/// - each create entry initial owner counts as 1,
/// - each config operation counts as 1.
pub fn account_change_units(tx: &TxEip8130) -> usize {
    tx.account_changes
        .iter()
        .map(|entry| match entry {
            AccountChangeEntry::Create(create) => 1 + create.initial_owners.len(),
            AccountChangeEntry::ConfigChange(cc) => cc.owner_changes.len(),
            AccountChangeEntry::Delegation(_) => 1,
        })
        .sum()
}

/// EIP-2028 calldata gas: 16 per non-zero byte, 4 per zero byte.
fn calldata_gas(data: &[u8]) -> u64 {
    data.iter().fold(0u64, |acc, &byte| acc + if byte == 0 { 4 } else { 16 })
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, Bytes, U256};

    use super::*;
    use crate::transaction::eip8130::{
        predeploys::{
            DELEGATE_VERIFIER_ADDRESS, K1_VERIFIER_ADDRESS, P256_RAW_VERIFIER_ADDRESS,
            P256_WEBAUTHN_VERIFIER_ADDRESS,
        },
        types::{ConfigChangeEntry, CreateEntry, Owner, OwnerChange},
    };

    #[test]
    fn calldata_gas_basic() {
        assert_eq!(calldata_gas(&[0, 0, 1, 2]), 4 + 4 + 16 + 16);
        assert_eq!(calldata_gas(&[]), 0);
    }

    #[test]
    fn nonce_key_cost_warm_vs_cold() {
        assert_eq!(nonce_key_cost(false), 22_100);
        assert_eq!(nonce_key_cost(true), 5_000);
    }

    #[test]
    fn bytecode_cost_no_create() {
        let tx = TxEip8130::default();
        assert_eq!(bytecode_cost(&tx), 0);
    }

    #[test]
    fn bytecode_cost_with_create() {
        let tx = TxEip8130 {
            account_changes: vec![AccountChangeEntry::Create(CreateEntry {
                user_salt: Default::default(),
                bytecode: Bytes::from_static(&[0x60; 100]),
                initial_owners: vec![Owner {
                    verifier: Address::repeat_byte(1),
                    owner_id: Default::default(),
                    scope: 0,
                }],
            })],
            ..Default::default()
        };
        assert_eq!(bytecode_cost(&tx), 32_000 + 200 * 100);
    }

    #[test]
    fn account_changes_cost_applied_vs_skipped() {
        let tx = TxEip8130 {
            account_changes: vec![
                AccountChangeEntry::ConfigChange(ConfigChangeEntry {
                    chain_id: 8453,
                    sequence: 0,
                    owner_changes: vec![
                        OwnerChange {
                            change_type: 0x01,
                            verifier: Address::repeat_byte(1),
                            owner_id: Default::default(),
                            scope: 0,
                        },
                        OwnerChange {
                            change_type: 0x01,
                            verifier: Address::repeat_byte(2),
                            owner_id: Default::default(),
                            scope: 0,
                        },
                    ],
                    authorizer_auth: Bytes::new(),
                }),
                AccountChangeEntry::ConfigChange(ConfigChangeEntry {
                    chain_id: 1, // different chain — will be skipped
                    sequence: 0,
                    owner_changes: vec![OwnerChange {
                        change_type: 0x01,
                        verifier: Address::repeat_byte(3),
                        owner_id: Default::default(),
                        scope: 0,
                    }],
                    authorizer_auth: Bytes::new(),
                }),
            ],
            ..Default::default()
        };
        let cost = account_changes_cost(&tx, 8453);
        assert_eq!(cost, 2 * CONFIG_CHANGE_OP_GAS + CONFIG_CHANGE_SKIP_GAS);
    }

    #[test]
    fn account_changes_cost_includes_create_and_initial_owners() {
        let tx = TxEip8130 {
            account_changes: vec![AccountChangeEntry::Create(CreateEntry {
                user_salt: B256::repeat_byte(0xAA),
                bytecode: Bytes::new(),
                initial_owners: vec![
                    Owner {
                        verifier: Address::repeat_byte(1),
                        owner_id: B256::repeat_byte(0x10),
                        scope: 0,
                    },
                    Owner {
                        verifier: Address::repeat_byte(2),
                        owner_id: B256::repeat_byte(0x11),
                        scope: 0,
                    },
                ],
            })],
            ..Default::default()
        };
        // create entry (1) + two initial owners (2) => 3 account-change units
        assert_eq!(account_changes_cost(&tx, 8453), 3 * CONFIG_CHANGE_OP_GAS);
    }

    #[test]
    fn account_change_units_counts_create_keys_and_config_ops() {
        let tx = TxEip8130 {
            account_changes: vec![
                AccountChangeEntry::Create(CreateEntry {
                    user_salt: B256::repeat_byte(0xAA),
                    bytecode: Bytes::new(),
                    initial_owners: vec![
                        Owner {
                            verifier: Address::repeat_byte(1),
                            owner_id: B256::repeat_byte(0x10),
                            scope: 0,
                        },
                        Owner {
                            verifier: Address::repeat_byte(2),
                            owner_id: B256::repeat_byte(0x11),
                            scope: 0,
                        },
                    ],
                }),
                AccountChangeEntry::ConfigChange(ConfigChangeEntry {
                    chain_id: 8453,
                    sequence: 0,
                    owner_changes: vec![
                        OwnerChange {
                            change_type: 0x01,
                            verifier: Address::repeat_byte(3),
                            owner_id: B256::repeat_byte(0x12),
                            scope: 0,
                        },
                        OwnerChange {
                            change_type: 0x02,
                            verifier: Address::repeat_byte(4),
                            owner_id: B256::repeat_byte(0x13),
                            scope: 0,
                        },
                    ],
                    authorizer_auth: Bytes::new(),
                }),
            ],
            ..Default::default()
        };
        // create(1) + initial owners(2) + config ops(2) = 5
        assert_eq!(account_change_units(&tx), 5);
    }

    #[test]
    fn sender_auth_cost_always_sload() {
        let mut tx = TxEip8130::default();
        tx.from = None; // EOA — still needs config check (revocation)
        assert_eq!(sender_auth_cost(&tx), SLOAD_GAS);

        tx.from = Some(Address::repeat_byte(1)); // Configured
        assert_eq!(sender_auth_cost(&tx), SLOAD_GAS);
    }

    #[test]
    fn payer_auth_cost_self_pay_vs_sponsored() {
        let mut tx = TxEip8130::default();
        assert_eq!(payer_auth_cost(&tx), 0); // self-pay

        tx.payer = Some(Address::repeat_byte(0xCC));
        assert_eq!(payer_auth_cost(&tx), SLOAD_GAS);
    }

    #[test]
    fn intrinsic_gas_smoke() {
        let mut auth = K1_VERIFIER_ADDRESS.as_slice().to_vec();
        auth.extend_from_slice(&[0xAB; 65]);
        let tx = TxEip8130 {
            chain_id: 8453,
            from: Some(Address::repeat_byte(1)),
            nonce_key: U256::ZERO,
            nonce_sequence: 0,
            sender_auth: Bytes::from(auth),
            ..Default::default()
        };
        let gas = intrinsic_gas(&tx, true, 8453);
        assert!(gas >= AA_BASE_COST, "intrinsic gas must be at least AA_BASE_COST");
    }

    #[test]
    fn sender_verification_gas_eoa_uses_k1() {
        let tx = TxEip8130 {
            from: None,
            sender_auth: Bytes::from_static(&[0xAB; 65]),
            ..Default::default()
        };
        let costs = VerifierGasCosts::BASE_V1;
        assert_eq!(sender_verification_gas(&tx, &costs, None), 6_000);
    }

    #[test]
    fn sender_verification_gas_configured_k1() {
        let mut auth = K1_VERIFIER_ADDRESS.as_slice().to_vec();
        auth.push(0xAB);
        let tx = TxEip8130 {
            from: Some(Address::repeat_byte(1)),
            sender_auth: Bytes::from(auth),
            ..Default::default()
        };
        assert_eq!(sender_verification_gas(&tx, &VerifierGasCosts::BASE_V1, None), 6_000);
    }

    #[test]
    fn sender_verification_gas_configured_p256() {
        let mut auth = P256_RAW_VERIFIER_ADDRESS.as_slice().to_vec();
        auth.push(0xAB);
        let tx = TxEip8130 {
            from: Some(Address::repeat_byte(1)),
            sender_auth: Bytes::from(auth),
            ..Default::default()
        };
        assert_eq!(sender_verification_gas(&tx, &VerifierGasCosts::BASE_V1, None), 9_500);
    }

    #[test]
    fn sender_verification_gas_configured_webauthn() {
        let mut auth = P256_WEBAUTHN_VERIFIER_ADDRESS.as_slice().to_vec();
        auth.push(0xAB);
        let tx = TxEip8130 {
            from: Some(Address::repeat_byte(1)),
            sender_auth: Bytes::from(auth),
            ..Default::default()
        };
        assert_eq!(sender_verification_gas(&tx, &VerifierGasCosts::BASE_V1, None), 15_000);
    }

    #[test]
    fn sender_verification_gas_delegate_with_k1_inner() {
        let mut auth = DELEGATE_VERIFIER_ADDRESS.as_slice().to_vec();
        auth.push(0xAB);
        let tx = TxEip8130 {
            from: Some(Address::repeat_byte(1)),
            sender_auth: Bytes::from(auth),
            ..Default::default()
        };
        let costs = VerifierGasCosts::BASE_V1;
        assert_eq!(sender_verification_gas(&tx, &costs, Some(K1_VERIFIER_ADDRESS)), 3_000 + 6_000,);
    }

    #[test]
    fn sender_verification_gas_delegate_with_p256_inner() {
        let mut auth = DELEGATE_VERIFIER_ADDRESS.as_slice().to_vec();
        auth.push(0xAB);
        let tx = TxEip8130 {
            from: Some(Address::repeat_byte(1)),
            sender_auth: Bytes::from(auth),
            ..Default::default()
        };
        let costs = VerifierGasCosts::BASE_V1;
        assert_eq!(
            sender_verification_gas(&tx, &costs, Some(P256_RAW_VERIFIER_ADDRESS)),
            3_000 + 9_500,
        );
    }

    #[test]
    fn sender_verification_gas_empty_auth_defaults_to_k1() {
        let tx = TxEip8130 {
            from: Some(Address::repeat_byte(1)),
            sender_auth: Bytes::new(),
            ..Default::default()
        };
        assert_eq!(sender_verification_gas(&tx, &VerifierGasCosts::BASE_V1, None), 6_000);
    }

    #[test]
    fn payer_verification_gas_empty_auth_defaults_to_k1() {
        let tx = TxEip8130 {
            payer: Some(Address::repeat_byte(0xCC)),
            payer_auth: Bytes::new(),
            ..Default::default()
        };
        assert_eq!(payer_verification_gas(&tx, &VerifierGasCosts::BASE_V1, None), 6_000);
    }

    #[test]
    fn sender_verification_gas_custom_is_zero() {
        let custom_verifier = Address::repeat_byte(0xCC);
        let mut auth = custom_verifier.as_slice().to_vec();
        auth.extend_from_slice(&[0xDD; 20]);
        let tx = TxEip8130 {
            from: Some(Address::repeat_byte(1)),
            sender_auth: Bytes::from(auth),
            ..Default::default()
        };
        assert_eq!(sender_verification_gas(&tx, &VerifierGasCosts::BASE_V1, None), 0);
    }

    #[test]
    fn payer_verification_gas_self_pay_is_zero() {
        let tx = TxEip8130::default();
        assert_eq!(payer_verification_gas(&tx, &VerifierGasCosts::BASE_V1, None), 0);
    }

    #[test]
    fn payer_verification_gas_sponsored_k1() {
        let mut auth = K1_VERIFIER_ADDRESS.as_slice().to_vec();
        auth.push(0xAB);
        let tx = TxEip8130 {
            payer: Some(Address::repeat_byte(0xCC)),
            payer_auth: Bytes::from(auth),
            ..Default::default()
        };
        assert_eq!(payer_verification_gas(&tx, &VerifierGasCosts::BASE_V1, None), 6_000);
    }

    #[test]
    fn total_verification_gas_sender_and_payer() {
        let mut payer_auth = P256_RAW_VERIFIER_ADDRESS.as_slice().to_vec();
        payer_auth.push(0xAB);
        let tx = TxEip8130 {
            from: None,
            sender_auth: Bytes::from_static(&[0xAB; 65]),
            payer: Some(Address::repeat_byte(0xCC)),
            payer_auth: Bytes::from(payer_auth),
            ..Default::default()
        };
        let costs = VerifierGasCosts::BASE_V1;
        assert_eq!(total_verification_gas(&tx, &costs, None, None), 6_000 + 9_500);
    }

    #[test]
    fn verifier_gas_costs_configurable() {
        let custom = VerifierGasCosts {
            k1: 5_000,
            p256_raw: 10_000,
            p256_webauthn: 20_000,
            delegate: 4_000,
        };
        assert_eq!(custom.gas_for_verifier(K1_VERIFIER_ADDRESS, None), 5_000);
        assert_eq!(custom.gas_for_verifier(P256_RAW_VERIFIER_ADDRESS, None), 10_000);
        assert_eq!(custom.gas_for_verifier(P256_WEBAUTHN_VERIFIER_ADDRESS, None), 20_000);
        assert_eq!(
            custom.gas_for_verifier(DELEGATE_VERIFIER_ADDRESS, Some(K1_VERIFIER_ADDRESS)),
            4_000 + 5_000,
        );
    }

    #[test]
    fn delegate_inner_verifier_extraction() {
        let mut delegate_k1 = DELEGATE_VERIFIER_ADDRESS.as_slice().to_vec();
        delegate_k1.extend_from_slice(Address::repeat_byte(0x11).as_slice());
        delegate_k1.extend_from_slice(K1_VERIFIER_ADDRESS.as_slice());
        delegate_k1.push(0xAB);
        assert_eq!(delegate_inner_verifier(&delegate_k1), Some(K1_VERIFIER_ADDRESS));

        let mut delegate_p256 = DELEGATE_VERIFIER_ADDRESS.as_slice().to_vec();
        delegate_p256.extend_from_slice(Address::repeat_byte(0x22).as_slice());
        delegate_p256.extend_from_slice(P256_RAW_VERIFIER_ADDRESS.as_slice());
        assert_eq!(delegate_inner_verifier(&delegate_p256), Some(P256_RAW_VERIFIER_ADDRESS));

        let mut k1_only = K1_VERIFIER_ADDRESS.as_slice().to_vec();
        k1_only.push(0xAB);
        assert_eq!(delegate_inner_verifier(&k1_only), None);

        let mut short_delegate = DELEGATE_VERIFIER_ADDRESS.as_slice().to_vec();
        short_delegate.extend_from_slice(K1_VERIFIER_ADDRESS.as_slice());
        assert_eq!(delegate_inner_verifier(&short_delegate), None);

        assert_eq!(delegate_inner_verifier(DELEGATE_VERIFIER_ADDRESS.as_slice()), None);
        assert_eq!(delegate_inner_verifier(&[]), None);
    }

    #[test]
    fn intrinsic_gas_includes_delegate_inner_cost() {
        let mut auth = DELEGATE_VERIFIER_ADDRESS.as_slice().to_vec();
        auth.extend_from_slice(Address::repeat_byte(0x33).as_slice());
        auth.extend_from_slice(K1_VERIFIER_ADDRESS.as_slice());
        auth.push(0xAB);
        let tx = TxEip8130 {
            chain_id: 8453,
            from: Some(Address::repeat_byte(1)),
            sender_auth: Bytes::from(auth),
            ..Default::default()
        };
        let costs = VerifierGasCosts::BASE_V1;
        let gas = intrinsic_gas(&tx, true, 8453);
        let expected_verification = costs.delegate + costs.k1; // 3k + 6k
        let base = AA_BASE_COST
            + tx_payload_cost(&tx)
            + sender_auth_cost(&tx)
            + payer_auth_cost(&tx)
            + nonce_key_cost(true)
            + bytecode_cost(&tx)
            + account_changes_cost(&tx, 8453);
        assert_eq!(gas, base + expected_verification);
    }

    #[test]
    fn intrinsic_gas_includes_verification_gas() {
        let mut auth = K1_VERIFIER_ADDRESS.as_slice().to_vec();
        auth.extend_from_slice(&[0xAB; 65]);
        let tx = TxEip8130 {
            chain_id: 8453,
            from: Some(Address::repeat_byte(1)),
            sender_auth: Bytes::from(auth),
            ..Default::default()
        };
        let without_verification = AA_BASE_COST
            + tx_payload_cost(&tx)
            + sender_auth_cost(&tx)
            + payer_auth_cost(&tx)
            + nonce_key_cost(true)
            + bytecode_cost(&tx)
            + account_changes_cost(&tx, 8453);
        let with_verification = intrinsic_gas(&tx, true, 8453);
        assert_eq!(with_verification, without_verification + 6_000);
    }
}
