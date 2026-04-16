//! Comprehensive integration tests for EIP-8130 Account Abstraction.
//!
//! These tests cover the full transaction lifecycle: construction, encoding,
//! validation, execution plan generation, and precompile handling. They
//! correspond to Milestone 6.2 scenarios in the implementation plan.

#[cfg(test)]
mod integration {
    use alloy_eips::eip2718::Typed2718;
    use alloy_primitives::{Address, B256, Bytes, U256};

    use crate::transaction::eip8130::{
        AA_BASE_COST, AA_TX_TYPE_ID, AccountChangeEntry, Call, ConfigChangeEntry, CreateEntry,
        Owner, OwnerChange, OwnerScope, TxEip8130,
        address::create2_address,
        gas::intrinsic_gas,
        signature::{parse_sender_auth, payer_signature_hash, sender_signature_hash},
    };

    fn simple_tx(from: Address) -> TxEip8130 {
        TxEip8130 {
            chain_id: 8453,
            from: Some(from),
            nonce_key: U256::ZERO,
            nonce_sequence: 0,
            expiry: 0,
            max_priority_fee_per_gas: 1_000_000_000,
            max_fee_per_gas: 2_000_000_000,
            gas_limit: 100_000,
            account_changes: vec![],
            calls: vec![vec![Call {
                to: Address::repeat_byte(0xBB),
                data: Bytes::from_static(&[0x01, 0x02]),
            }]],
            payer: None,
            sender_auth: Bytes::from(vec![0u8; 65]),
            payer_auth: Bytes::default(),
        }
    }

    // -----------------------------------------------------------------------
    // Scenario 1: EOA K1 self-pay — sign, encode, decode
    // -----------------------------------------------------------------------

    #[test]
    fn eoa_k1_self_pay_roundtrip() {
        let from = Address::repeat_byte(0xAA);
        let tx = simple_tx(from);

        assert_eq!(tx.ty(), AA_TX_TYPE_ID);
        assert_eq!(tx.effective_sender(), from);

        let mut buf = Vec::new();
        tx.rlp_encode(&mut buf);
        let decoded = TxEip8130::rlp_decode(&mut buf.as_slice()).unwrap();
        assert_eq!(tx, decoded);
    }

    #[test]
    fn eoa_k1_self_pay_eip2718_roundtrip() {
        use alloy_eips::eip2718::{Decodable2718, Encodable2718};
        let tx = simple_tx(Address::repeat_byte(0xAA));

        let mut buf = Vec::new();
        tx.encode_2718(&mut buf);
        assert_eq!(buf[0], AA_TX_TYPE_ID);

        let decoded = TxEip8130::decode_2718(&mut buf.as_slice()).unwrap();
        assert_eq!(tx, decoded);
    }

    // -----------------------------------------------------------------------
    // Scenario 2: Sponsored gas — payer differs from sender
    // -----------------------------------------------------------------------

    #[test]
    fn sponsored_tx_payer_set() {
        let from = Address::repeat_byte(0xAA);
        let payer = Address::repeat_byte(0xCC);
        let tx = TxEip8130 {
            payer: Some(payer),
            payer_auth: Bytes::from(vec![0u8; 65]),
            ..simple_tx(from)
        };

        assert_eq!(tx.payer, Some(payer));
        assert!(!tx.payer_auth.is_empty());

        let sender_hash = sender_signature_hash(&tx);
        let payer_hash = payer_signature_hash(&tx);
        assert_ne!(sender_hash, payer_hash);
    }

    // -----------------------------------------------------------------------
    // Scenario 3: Account creation with initial owners
    // -----------------------------------------------------------------------

    #[test]
    fn account_creation_entry_roundtrip() {
        let from = Address::repeat_byte(0x42);
        let tx = TxEip8130 {
            account_changes: vec![AccountChangeEntry::Create(CreateEntry {
                user_salt: B256::repeat_byte(0x01),
                bytecode: Bytes::from_static(&[0x60, 0x80, 0x60, 0x40]),
                initial_owners: vec![
                    Owner {
                        verifier: Address::repeat_byte(0x01),
                        owner_id: B256::repeat_byte(0x02),
                        scope: OwnerScope::UNRESTRICTED,
                    },
                    Owner {
                        verifier: Address::repeat_byte(0x03),
                        owner_id: B256::repeat_byte(0x04),
                        scope: OwnerScope::SENDER | OwnerScope::PAYER,
                    },
                ],
            })],
            ..simple_tx(from)
        };

        let mut buf = Vec::new();
        tx.rlp_encode(&mut buf);
        let decoded = TxEip8130::rlp_decode(&mut buf.as_slice()).unwrap();
        assert_eq!(tx, decoded);
    }

    #[test]
    fn create2_address_derivation() {
        let deployer = Address::repeat_byte(0x42);
        let salt = B256::repeat_byte(0x01);
        let code = [0x60, 0x80];
        let addr = create2_address(deployer, salt, &code);
        assert_ne!(addr, Address::ZERO);

        let addr2 = create2_address(deployer, B256::repeat_byte(0x02), &code);
        assert_ne!(addr, addr2);
    }

    // -----------------------------------------------------------------------
    // Scenario 4: Config change — authorize + revoke
    // -----------------------------------------------------------------------

    #[test]
    fn config_change_entry_roundtrip() {
        let from = Address::repeat_byte(0x42);
        let tx = TxEip8130 {
            account_changes: vec![AccountChangeEntry::ConfigChange(ConfigChangeEntry {
                chain_id: 8453,
                sequence: 3,
                owner_changes: vec![
                    OwnerChange {
                        change_type: 0x01,
                        verifier: Address::repeat_byte(0x01),
                        owner_id: B256::repeat_byte(0x99),
                        scope: OwnerScope::SENDER,
                    },
                    OwnerChange {
                        change_type: 0x02,
                        verifier: Address::ZERO,
                        owner_id: B256::repeat_byte(0x88),
                        scope: 0,
                    },
                ],
                authorizer_auth: Bytes::from(vec![0xFF; 65]),
            })],
            ..simple_tx(from)
        };

        let mut buf = Vec::new();
        tx.rlp_encode(&mut buf);
        let decoded = TxEip8130::rlp_decode(&mut buf.as_slice()).unwrap();
        assert_eq!(tx, decoded);
    }

    // -----------------------------------------------------------------------
    // Scenario 5: 2D nonce — parallel txs on different channels
    // -----------------------------------------------------------------------

    #[test]
    fn two_d_nonce_different_keys() {
        let from = Address::repeat_byte(0xAA);
        let tx1 = TxEip8130 { nonce_key: U256::ZERO, nonce_sequence: 5, ..simple_tx(from) };
        let tx2 = TxEip8130 { nonce_key: U256::from(1), nonce_sequence: 0, ..simple_tx(from) };

        assert_ne!(tx1.nonce_key, tx2.nonce_key);
        assert_ne!(sender_signature_hash(&tx1), sender_signature_hash(&tx2));
    }

    // -----------------------------------------------------------------------
    // Scenario 6: Expiry enforcement
    // -----------------------------------------------------------------------

    #[test]
    fn expiry_set_and_roundtripped() {
        let tx = TxEip8130 { expiry: 1000, ..simple_tx(Address::repeat_byte(0xAA)) };

        let mut buf = Vec::new();
        tx.rlp_encode(&mut buf);
        let decoded = TxEip8130::rlp_decode(&mut buf.as_slice()).unwrap();
        assert_eq!(decoded.expiry, 1000);
    }

    // -----------------------------------------------------------------------
    // Scenario 7: Auto-delegation (bare EOA: from = Address::ZERO)
    // -----------------------------------------------------------------------

    #[test]
    fn auto_delegation_zero_from() {
        let tx = TxEip8130 { from: None, ..simple_tx(Address::repeat_byte(0xAA)) };
        assert!(tx.from.is_none());
    }

    // -----------------------------------------------------------------------
    // Scenario 8: Phased calls — multiple phases
    // -----------------------------------------------------------------------

    #[test]
    fn phased_calls_structure() {
        let tx = TxEip8130 {
            calls: vec![
                vec![Call { to: Address::repeat_byte(0x01), data: Bytes::from_static(&[0x01]) }],
                vec![
                    Call { to: Address::repeat_byte(0x02), data: Bytes::from_static(&[0x02]) },
                    Call { to: Address::repeat_byte(0x03), data: Bytes::from_static(&[0x03]) },
                ],
            ],
            ..simple_tx(Address::repeat_byte(0xAA))
        };

        assert_eq!(tx.calls.len(), 2);
        assert_eq!(tx.calls[1].len(), 2);

        let mut buf = Vec::new();
        tx.rlp_encode(&mut buf);
        let decoded = TxEip8130::rlp_decode(&mut buf.as_slice()).unwrap();
        assert_eq!(tx, decoded);
    }

    // -----------------------------------------------------------------------
    // Scenario 9: Owner scope enforcement
    // -----------------------------------------------------------------------

    #[test]
    fn owner_scope_permissions() {
        assert!(OwnerScope::has(OwnerScope::UNRESTRICTED, OwnerScope::SENDER));
        assert!(OwnerScope::has(OwnerScope::UNRESTRICTED, OwnerScope::PAYER));
        assert!(OwnerScope::has(OwnerScope::UNRESTRICTED, OwnerScope::CONFIG));

        let sender_only = OwnerScope::SENDER;
        assert!(OwnerScope::has(sender_only, OwnerScope::SENDER));
        assert!(!OwnerScope::has(sender_only, OwnerScope::PAYER));

        let multi = OwnerScope::SENDER | OwnerScope::PAYER;
        assert!(OwnerScope::has(multi, OwnerScope::SENDER));
        assert!(OwnerScope::has(multi, OwnerScope::PAYER));
        assert!(!OwnerScope::has(multi, OwnerScope::CONFIG));
    }

    // -----------------------------------------------------------------------
    // Scenario 10: Gas calculation
    // -----------------------------------------------------------------------

    #[test]
    fn intrinsic_gas_base() {
        let gas = intrinsic_gas(&simple_tx(Address::repeat_byte(0xAA)), true, 8453);
        assert!(gas >= AA_BASE_COST);
    }

    #[test]
    fn intrinsic_gas_cold_nonce_higher() {
        let tx = simple_tx(Address::repeat_byte(0xAA));
        let warm = intrinsic_gas(&tx, true, 8453);
        let cold = intrinsic_gas(&tx, false, 8453);
        assert!(cold > warm);
    }

    // -----------------------------------------------------------------------
    // Scenario 11: Signature hashes
    // -----------------------------------------------------------------------

    #[test]
    fn sender_and_payer_hashes_differ() {
        let tx = TxEip8130 {
            payer: Some(Address::repeat_byte(0xBB)),
            payer_auth: Bytes::from(vec![0u8; 65]),
            ..simple_tx(Address::repeat_byte(0xAA))
        };
        assert_ne!(sender_signature_hash(&tx), payer_signature_hash(&tx));
    }

    #[test]
    fn sender_hash_deterministic() {
        let tx = simple_tx(Address::repeat_byte(0xAA));
        assert_eq!(sender_signature_hash(&tx), sender_signature_hash(&tx));
    }

    #[test]
    fn sender_hash_changes_with_nonce() {
        let from = Address::repeat_byte(0xAA);
        let tx1 = TxEip8130 { nonce_sequence: 0, ..simple_tx(from) };
        let tx2 = TxEip8130 { nonce_sequence: 1, ..simple_tx(from) };
        assert_ne!(sender_signature_hash(&tx1), sender_signature_hash(&tx2));
    }

    // -----------------------------------------------------------------------
    // Scenario 12: Parse sender auth
    // -----------------------------------------------------------------------

    #[test]
    fn parse_eoa_sender_auth() {
        let tx = simple_tx(Address::repeat_byte(0xAA));
        assert!(parse_sender_auth(&tx).is_ok());
    }

    #[test]
    fn parse_eoa_sender_auth_too_short() {
        let tx = TxEip8130 {
            from: None,
            sender_auth: Bytes::from(vec![0u8; 64]),
            ..simple_tx(Address::ZERO)
        };
        assert!(parse_sender_auth(&tx).is_err());
    }

    // -----------------------------------------------------------------------
    // Scenario 13: AA tx type distinct
    // -----------------------------------------------------------------------

    #[test]
    fn aa_tx_type_distinct() {
        assert_eq!(AA_TX_TYPE_ID, 0x7B);
        assert_ne!(AA_TX_TYPE_ID, 0x00);
        assert_ne!(AA_TX_TYPE_ID, 0x01);
        assert_ne!(AA_TX_TYPE_ID, 0x02);
        assert_ne!(AA_TX_TYPE_ID, 0x04);
        assert_ne!(AA_TX_TYPE_ID, 0x7E);
    }

    // -----------------------------------------------------------------------
    // Scenario 14: Predeploy address uniqueness
    // -----------------------------------------------------------------------

    #[test]
    fn predeploy_addresses_unique() {
        use crate::transaction::eip8130::predeploys::*;
        let addrs = [
            ACCOUNT_CONFIG_ADDRESS,
            NONCE_MANAGER_ADDRESS,
            TX_CONTEXT_ADDRESS,
            DEFAULT_ACCOUNT_ADDRESS,
            K1_VERIFIER_ADDRESS,
            P256_RAW_VERIFIER_ADDRESS,
            P256_WEBAUTHN_VERIFIER_ADDRESS,
            DELEGATE_VERIFIER_ADDRESS,
        ];
        for (i, a) in addrs.iter().enumerate() {
            for (j, b) in addrs.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "predeploy addresses must be unique");
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Scenario 15: Storage slot derivation
    // -----------------------------------------------------------------------

    #[test]
    fn storage_slots_deterministic() {
        use crate::transaction::eip8130::storage::*;

        let account = Address::repeat_byte(0x42);
        let owner_id = B256::repeat_byte(0x01);
        assert_eq!(owner_config_slot(account, owner_id), owner_config_slot(account, owner_id));
        assert_ne!(
            owner_config_slot(account, owner_id),
            owner_config_slot(account, B256::repeat_byte(0x02))
        );
    }

    #[test]
    fn owner_config_pack_unpack() {
        use crate::transaction::eip8130::storage::*;

        let verifier = Address::repeat_byte(0xAB);
        let scope: u8 = OwnerScope::SENDER | OwnerScope::PAYER;
        let (dv, ds) = parse_owner_config(encode_owner_config(verifier, scope));
        assert_eq!(dv, verifier);
        assert_eq!(ds, scope);
    }

    // -----------------------------------------------------------------------
    // Scenario 16-18: Edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn empty_calls_roundtrip() {
        let tx = TxEip8130 { calls: vec![], ..simple_tx(Address::repeat_byte(0xAA)) };
        let mut buf = Vec::new();
        tx.rlp_encode(&mut buf);
        assert_eq!(tx.calls, TxEip8130::rlp_decode(&mut buf.as_slice()).unwrap().calls);
    }

    #[test]
    fn large_nonce_key() {
        let tx =
            TxEip8130 { nonce_key: U256::from(u64::MAX), ..simple_tx(Address::repeat_byte(0xAA)) };
        let mut buf = Vec::new();
        tx.rlp_encode(&mut buf);
        assert_eq!(
            TxEip8130::rlp_decode(&mut buf.as_slice()).unwrap().nonce_key,
            U256::from(u64::MAX)
        );
    }

    // -----------------------------------------------------------------------
    // Scenario 20: Tx hash uniqueness
    // -----------------------------------------------------------------------

    #[test]
    fn tx_hash_uniqueness() {
        use alloy_primitives::Sealable;
        let from = Address::repeat_byte(0xAA);
        let h1 = simple_tx(from).hash_slow();
        let h2 = TxEip8130 { nonce_sequence: 1, ..simple_tx(from) }.hash_slow();
        assert_ne!(h1, h2);
        assert_ne!(h1, B256::ZERO);
    }

    // -----------------------------------------------------------------------
    // Scenario 21: Complex tx with all features
    // -----------------------------------------------------------------------

    #[test]
    fn complex_tx_with_all_features() {
        let tx = TxEip8130 {
            chain_id: 8453,
            from: Some(Address::repeat_byte(0x42)),
            nonce_key: U256::from(42),
            nonce_sequence: 7,
            expiry: 1_700_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            max_fee_per_gas: 50_000_000_000,
            gas_limit: 5_000_000,
            account_changes: vec![
                AccountChangeEntry::Create(CreateEntry {
                    user_salt: B256::repeat_byte(0xAA),
                    bytecode: Bytes::from(vec![0x60; 32]),
                    initial_owners: vec![Owner {
                        verifier: Address::repeat_byte(0x01),
                        owner_id: B256::repeat_byte(0x02),
                        scope: OwnerScope::UNRESTRICTED,
                    }],
                }),
                AccountChangeEntry::ConfigChange(ConfigChangeEntry {
                    chain_id: 0,
                    sequence: 1,
                    owner_changes: vec![OwnerChange {
                        change_type: 0x01,
                        verifier: Address::repeat_byte(0x03),
                        owner_id: B256::repeat_byte(0x04),
                        scope: OwnerScope::SENDER | OwnerScope::CONFIG,
                    }],
                    authorizer_auth: Bytes::from(vec![0xFF; 65]),
                }),
            ],
            calls: vec![
                vec![Call { to: Address::repeat_byte(0x10), data: Bytes::from(vec![0x01; 100]) }],
                vec![
                    Call { to: Address::repeat_byte(0x20), data: Bytes::from(vec![0x02; 50]) },
                    Call { to: Address::repeat_byte(0x30), data: Bytes::default() },
                ],
            ],
            payer: Some(Address::repeat_byte(0xCC)),
            sender_auth: Bytes::from(vec![0xAA; 65]),
            payer_auth: Bytes::from(vec![0xBB; 65]),
        };

        let mut buf = Vec::new();
        tx.rlp_encode(&mut buf);
        assert_eq!(tx, TxEip8130::rlp_decode(&mut buf.as_slice()).unwrap());
        assert!(intrinsic_gas(&tx, true, 8453) > AA_BASE_COST);
    }
}

#[cfg(all(test, feature = "evm"))]
mod evm_integration {
    use alloy_primitives::{Address, B256, Bytes, U256};

    use crate::transaction::eip8130::{
        Call, TxEip8130,
        execution::{
            TxContextValues, auto_delegation_code, build_execution_calls, gas_refund,
            max_execution_gas_cost, nonce_increment_write,
        },
        precompiles::{PrecompileError, TX_CONTEXT_GAS, handle_tx_context},
        predeploys::NONCE_MANAGER_ADDRESS,
        validation::{validate_expiry, validate_structure},
    };

    fn simple_tx(from: Address) -> TxEip8130 {
        TxEip8130 {
            chain_id: 8453,
            from: Some(from),
            nonce_key: U256::ZERO,
            nonce_sequence: 0,
            expiry: 0,
            max_priority_fee_per_gas: 1_000_000_000,
            max_fee_per_gas: 2_000_000_000,
            gas_limit: 100_000,
            account_changes: vec![],
            calls: vec![vec![Call {
                to: Address::repeat_byte(0xBB),
                data: Bytes::from_static(&[0x01, 0x02]),
            }]],
            payer: None,
            sender_auth: Bytes::from(vec![0u8; 65]),
            payer_auth: Bytes::default(),
        }
    }

    #[test]
    fn validate_structure_valid() {
        assert!(validate_structure(&simple_tx(Address::repeat_byte(0xAA))).is_ok());
    }

    #[test]
    fn validate_structure_oversized_sender_auth() {
        let tx = TxEip8130 {
            sender_auth: Bytes::from(vec![0u8; 3000]),
            ..simple_tx(Address::repeat_byte(0xAA))
        };
        assert!(validate_structure(&tx).is_err());
    }

    #[test]
    fn validate_expiry_zero_always_ok() {
        assert!(validate_expiry(&simple_tx(Address::repeat_byte(0xAA)), u64::MAX).is_ok());
    }

    #[test]
    fn validate_expiry_future_ok() {
        let tx = TxEip8130 { expiry: 2_000_000_000, ..simple_tx(Address::repeat_byte(0xAA)) };
        assert!(validate_expiry(&tx, 1_000_000_000).is_ok());
    }

    #[test]
    fn validate_expiry_past_err() {
        let tx = TxEip8130 { expiry: 1_000_000_000, ..simple_tx(Address::repeat_byte(0xAA)) };
        assert!(validate_expiry(&tx, 2_000_000_000).is_err());
    }

    #[test]
    fn execution_calls_single() {
        let sender = Address::repeat_byte(0xAA);
        let calls = build_execution_calls(&simple_tx(sender), sender);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].len(), 1);
    }

    #[test]
    fn execution_calls_multi() {
        let tx = TxEip8130 {
            calls: vec![
                vec![Call { to: Address::repeat_byte(0x01), data: Bytes::default() }],
                vec![
                    Call { to: Address::repeat_byte(0x02), data: Bytes::default() },
                    Call { to: Address::repeat_byte(0x03), data: Bytes::default() },
                ],
            ],
            ..simple_tx(Address::repeat_byte(0xAA))
        };
        let calls = build_execution_calls(&tx, tx.from.unwrap_or(Address::ZERO));
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].len(), 2);
    }

    #[test]
    fn nonce_write_contract() {
        let write = nonce_increment_write(Address::repeat_byte(0xAA), U256::ZERO, 1);
        assert_eq!(write.address, NONCE_MANAGER_ADDRESS);
    }

    #[test]
    fn max_cost() {
        let tx = simple_tx(Address::repeat_byte(0xAA));
        assert_eq!(
            max_execution_gas_cost(&tx),
            U256::from(tx.max_fee_per_gas) * U256::from(tx.gas_limit)
        );
    }

    #[test]
    fn refund_positive() {
        assert!(gas_refund(100_000, 50_000, 1_500_000_000) > U256::ZERO);
    }

    #[test]
    fn auto_delegation_code_nonempty() {
        assert!(!auto_delegation_code().is_empty());
    }

    #[test]
    fn tx_context_getters() {
        use crate::transaction::eip8130::abi::ITxContext;
        use alloy_sol_types::SolCall;

        let ctx = TxContextValues {
            sender: Address::repeat_byte(0xAA),
            payer: Address::repeat_byte(0xBB),
            owner_id: B256::repeat_byte(0xCC),
            gas_limit: 1_000_000,
            max_cost: U256::from(2_000_000_000_000u64),
            calls: Vec::new(),
        };

        for sel in [
            ITxContext::getSenderCall::SELECTOR,
            ITxContext::getPayerCall::SELECTOR,
            ITxContext::getOwnerIdCall::SELECTOR,
            ITxContext::getMaxCostCall::SELECTOR,
            ITxContext::getGasLimitCall::SELECTOR,
            ITxContext::getCallsCall::SELECTOR,
        ] {
            let (gas, out) = handle_tx_context(&ctx, &sel).unwrap();
            assert_eq!(gas, TX_CONTEXT_GAS);
            assert!(!out.is_empty());
        }
    }

    #[test]
    fn tx_context_bad_selector() {
        assert!(matches!(
            handle_tx_context(&TxContextValues::default(), &[0xDE, 0xAD, 0xBE, 0xEF]),
            Err(PrecompileError::UnknownSelector)
        ));
    }
}
