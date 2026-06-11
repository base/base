//! B-20 precompile action tests across the Base Beryl boundary.

use alloy_consensus::TxReceipt;
use alloy_primitives::{Address, B256, Bytes, FixedBytes, TxKind, U256, keccak256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, SolEvent, SolValue};
use base_action_harness::TEST_ACCOUNT_KEY;
use base_common_consensus::{BaseBlock, BaseTxEnvelope};
use base_common_precompiles::{B20_MAX_SUPPLY_CAP, B20TokenRole, IB20};

use crate::env::BerylTestEnv;

const PERMIT_TYPE: &[u8] =
    b"Permit(address owner,address spender,uint256 value,uint256 nonce,uint256 deadline)";
const DOMAIN_TYPE: &[u8] =
    b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)";
const DOMAIN_VERSION: &[u8] = b"1";
const MEMO_TRANSFER: B256 = B256::repeat_byte(0x10);
const MEMO_TRANSFER_FROM: B256 = B256::repeat_byte(0x11);
const MEMO_MINT: B256 = B256::repeat_byte(0x12);
const MEMO_BURN: B256 = B256::repeat_byte(0x13);

#[tokio::test]
async fn b20_transfers_update_balances_and_emit_events() {
    let mut scenario = B20TokenScenario::new().await;

    let transfer_to_bob = scenario.env.transfer_b20_tx(
        scenario.token,
        BerylTestEnv::bob(),
        U256::from(BerylTestEnv::B20_BOB_TRANSFER),
    );
    let block = scenario.build_block_with_transactions(vec![transfer_to_bob]).await;

    assert!(scenario.env.user_tx_succeeded(&block, 0), "Alice transfer transaction must succeed");
    scenario.assert_transfer_log(
        &block,
        BerylTestEnv::alice(),
        BerylTestEnv::bob(),
        BerylTestEnv::B20_BOB_TRANSFER,
    );
    scenario.assert_total_supply(BerylTestEnv::B20_INITIAL_SUPPLY);
    scenario.assert_balances(
        BerylTestEnv::B20_INITIAL_SUPPLY - BerylTestEnv::B20_BOB_TRANSFER,
        BerylTestEnv::B20_BOB_TRANSFER,
        0,
    );

    let transfer_to_carol = scenario.env.transfer_b20_from_bob_tx(
        scenario.token,
        BerylTestEnv::carol(),
        U256::from(BerylTestEnv::B20_CAROL_TRANSFER),
    );
    let block = scenario.build_block_with_transactions(vec![transfer_to_carol]).await;

    assert!(scenario.env.user_tx_succeeded(&block, 0), "Bob transfer transaction must succeed");
    scenario.assert_transfer_log(
        &block,
        BerylTestEnv::bob(),
        BerylTestEnv::carol(),
        BerylTestEnv::B20_CAROL_TRANSFER,
    );
    scenario.assert_total_supply(BerylTestEnv::B20_INITIAL_SUPPLY);
    scenario.assert_balances(
        BerylTestEnv::B20_INITIAL_SUPPLY - BerylTestEnv::B20_BOB_TRANSFER,
        BerylTestEnv::B20_BOB_TRANSFER - BerylTestEnv::B20_CAROL_TRANSFER,
        BerylTestEnv::B20_CAROL_TRANSFER,
    );

    scenario.derive().await;
}

#[tokio::test]
async fn b20_transfer_reverts_when_sender_balance_is_insufficient() {
    let mut scenario = B20TokenScenario::new().await;

    let transfer_to_bob = scenario.env.transfer_b20_tx(
        scenario.token,
        BerylTestEnv::bob(),
        U256::from(BerylTestEnv::B20_BOB_TRANSFER),
    );
    let block = scenario.build_block_with_transactions(vec![transfer_to_bob]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "Alice transfer transaction must succeed");

    let overdraw_amount = BerylTestEnv::B20_BOB_TRANSFER + 1;
    let overdraw = scenario.env.transfer_b20_from_bob_tx(
        scenario.token,
        BerylTestEnv::carol(),
        U256::from(overdraw_amount),
    );
    let block = scenario.build_block_with_transactions(vec![overdraw]).await;

    assert!(!scenario.env.user_tx_succeeded(&block, 0), "Bob overdraw transfer must revert");
    assert!(
        !scenario.env.b20_transfer_log_emitted(
            &block,
            0,
            scenario.token,
            BerylTestEnv::bob(),
            BerylTestEnv::carol(),
            U256::from(overdraw_amount),
        ),
        "failed overdraw transfer must not emit a Transfer event"
    );
    scenario.assert_balances(
        BerylTestEnv::B20_INITIAL_SUPPLY - BerylTestEnv::B20_BOB_TRANSFER,
        BerylTestEnv::B20_BOB_TRANSFER,
        0,
    );

    scenario.derive().await;
}

#[tokio::test]
async fn b20_approval_and_transfer_from_update_allowance() {
    let mut scenario = B20TokenScenario::new().await;

    let approve_bob = scenario.env.approve_b20_tx(
        scenario.token,
        BerylTestEnv::bob(),
        U256::from(BerylTestEnv::B20_BOB_ALLOWANCE),
    );
    let block = scenario.build_block_with_transactions(vec![approve_bob]).await;

    assert!(scenario.env.user_tx_succeeded(&block, 0), "Alice approval transaction must succeed");
    scenario.assert_approval_log(
        &block,
        BerylTestEnv::alice(),
        BerylTestEnv::bob(),
        BerylTestEnv::B20_BOB_ALLOWANCE,
    );
    scenario.assert_allowance(
        BerylTestEnv::alice(),
        BerylTestEnv::bob(),
        BerylTestEnv::B20_BOB_ALLOWANCE,
    );

    let transfer_from_alice_to_carol = scenario.env.transfer_b20_from_alice_by_bob_tx(
        scenario.token,
        BerylTestEnv::carol(),
        U256::from(BerylTestEnv::B20_TRANSFER_FROM_CAROL),
    );
    let block = scenario.build_block_with_transactions(vec![transfer_from_alice_to_carol]).await;

    assert!(scenario.env.user_tx_succeeded(&block, 0), "Bob transferFrom transaction must succeed");
    scenario.assert_transfer_log(
        &block,
        BerylTestEnv::alice(),
        BerylTestEnv::carol(),
        BerylTestEnv::B20_TRANSFER_FROM_CAROL,
    );
    scenario.assert_balances(
        BerylTestEnv::B20_INITIAL_SUPPLY - BerylTestEnv::B20_TRANSFER_FROM_CAROL,
        0,
        BerylTestEnv::B20_TRANSFER_FROM_CAROL,
    );
    scenario.assert_allowance(
        BerylTestEnv::alice(),
        BerylTestEnv::bob(),
        BerylTestEnv::B20_BOB_ALLOWANCE - BerylTestEnv::B20_TRANSFER_FROM_CAROL,
    );
    scenario.assert_total_supply(BerylTestEnv::B20_INITIAL_SUPPLY);

    scenario.derive().await;
}

#[tokio::test]
async fn b20_staticcall_abi_returns_storage_values() {
    let mut scenario = B20TokenScenario::new().await;

    let transfer_to_bob = scenario.env.transfer_b20_tx(
        scenario.token,
        BerylTestEnv::bob(),
        U256::from(BerylTestEnv::B20_BOB_TRANSFER),
    );
    let approve_bob = scenario.env.approve_b20_tx(
        scenario.token,
        BerylTestEnv::bob(),
        U256::from(BerylTestEnv::B20_BOB_ALLOWANCE),
    );
    let transfer_from_alice_to_carol = scenario.env.transfer_b20_from_alice_by_bob_tx(
        scenario.token,
        BerylTestEnv::carol(),
        U256::from(BerylTestEnv::B20_TRANSFER_FROM_CAROL),
    );
    let block = scenario
        .build_block_with_transactions(vec![
            transfer_to_bob,
            approve_bob,
            transfer_from_alice_to_carol,
        ])
        .await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "Alice transfer transaction must succeed");
    assert!(scenario.env.user_tx_succeeded(&block, 1), "Alice approval transaction must succeed");
    assert!(scenario.env.user_tx_succeeded(&block, 2), "Bob transferFrom transaction must succeed");

    let probes = B20StaticcallProbes::deploy(&mut scenario).await;
    let probe_calls = probes.call_txs(&scenario);
    let _block = scenario.build_block_with_transactions(probe_calls).await;

    probes.assert_returns(
        &scenario,
        B20ProbeExpectations {
            total_supply: BerylTestEnv::B20_INITIAL_SUPPLY,
            alice_balance: BerylTestEnv::B20_INITIAL_SUPPLY
                - BerylTestEnv::B20_BOB_TRANSFER
                - BerylTestEnv::B20_TRANSFER_FROM_CAROL,
            bob_balance: BerylTestEnv::B20_BOB_TRANSFER,
            carol_balance: BerylTestEnv::B20_TRANSFER_FROM_CAROL,
            allowance: BerylTestEnv::B20_BOB_ALLOWANCE - BerylTestEnv::B20_TRANSFER_FROM_CAROL,
            decimals: BerylTestEnv::B20_DECIMALS,
        },
    );

    scenario.derive().await;
}

#[tokio::test]
async fn b20_transfer_succeeds_while_token_feature_is_deactivated() {
    let mut scenario = B20TokenScenario::new().await;

    let deactivate_b20 = scenario.env.deactivate_feature_tx(BerylTestEnv::b20_asset_feature());
    let block = scenario.build_block_with_transactions(vec![deactivate_b20]).await;

    assert!(scenario.env.user_tx_succeeded(&block, 0), "B20_ASSET deactivation must succeed");

    let transfer_while_deactivated =
        scenario.env.transfer_b20_tx(scenario.token, BerylTestEnv::bob(), U256::from(1));
    let block = scenario.build_block_with_transactions(vec![transfer_while_deactivated]).await;

    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "existing token transfer must succeed even when B20_ASSET is deactivated"
    );
    scenario.assert_transfer_log(&block, BerylTestEnv::alice(), BerylTestEnv::bob(), 1);
    scenario.assert_balances(BerylTestEnv::B20_INITIAL_SUPPLY - 1, 1, 0);
    scenario.assert_total_supply(BerylTestEnv::B20_INITIAL_SUPPLY);

    scenario.derive().await;
}

#[tokio::test]
async fn b20_staticcall_abi_covers_all_read_methods() {
    let mut scenario = B20TokenScenario::new().await;

    let approve_bob = scenario.env.approve_b20_tx(
        scenario.token,
        BerylTestEnv::bob(),
        U256::from(BerylTestEnv::B20_BOB_ALLOWANCE),
    );
    let block = scenario.build_block_with_transactions(vec![approve_bob]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "Alice approval transaction must succeed");

    scenario
        .assert_staticcall_cases(vec![
            StaticcallCase::output(
                "name",
                IB20::nameCall {}.abi_encode(),
                IB20::nameCall::abi_encode_returns(&BerylTestEnv::B20_NAME.to_string()),
            ),
            StaticcallCase::output(
                "symbol",
                IB20::symbolCall {}.abi_encode(),
                IB20::symbolCall::abi_encode_returns(&BerylTestEnv::B20_SYMBOL.to_string()),
            ),
            StaticcallCase::word(
                "decimals",
                IB20::decimalsCall {}.abi_encode(),
                U256::from(BerylTestEnv::B20_DECIMALS),
            ),
            StaticcallCase::word(
                "totalSupply",
                IB20::totalSupplyCall {}.abi_encode(),
                U256::from(BerylTestEnv::B20_INITIAL_SUPPLY),
            ),
            StaticcallCase::word(
                "balanceOf",
                IB20::balanceOfCall { account: BerylTestEnv::alice() }.abi_encode(),
                U256::from(BerylTestEnv::B20_INITIAL_SUPPLY),
            ),
            StaticcallCase::word(
                "allowance",
                IB20::allowanceCall { owner: BerylTestEnv::alice(), spender: BerylTestEnv::bob() }
                    .abi_encode(),
                U256::from(BerylTestEnv::B20_BOB_ALLOWANCE),
            ),
            StaticcallCase::word(
                "pausedFeatures",
                IB20::pausedFeaturesCall {}.abi_encode(),
                U256::from(32),
            )
            .with_output(IB20::pausedFeaturesCall::abi_encode_returns(
                &Vec::<IB20::PausableFeature>::new(),
            )),
            StaticcallCase::word(
                "isPaused",
                IB20::isPausedCall { feature: IB20::PausableFeature::TRANSFER }.abi_encode(),
                U256::ZERO,
            ),
            StaticcallCase::word(
                "supplyCap",
                IB20::supplyCapCall {}.abi_encode(),
                B20_MAX_SUPPLY_CAP,
            ),
            StaticcallCase::word(
                "DOMAIN_SEPARATOR",
                IB20::DOMAIN_SEPARATORCall {}.abi_encode(),
                domain_separator_word(
                    scenario.env.chain_id(),
                    scenario.token,
                    BerylTestEnv::B20_NAME,
                ),
            ),
            StaticcallCase::word(
                "nonces",
                IB20::noncesCall { owner: BerylTestEnv::alice() }.abi_encode(),
                U256::ZERO,
            ),
            StaticcallCase::word(
                "eip712Domain",
                IB20::eip712DomainCall {}.abi_encode(),
                eip712_domain_fields_word(),
            )
            .with_output(IB20::eip712DomainCall::abi_encode_returns(
                &eip712_domain_return(
                    scenario.env.chain_id(),
                    scenario.token,
                    BerylTestEnv::B20_NAME,
                ),
            )),
            StaticcallCase::output(
                "contractURI",
                IB20::contractURICall {}.abi_encode(),
                IB20::contractURICall::abi_encode_returns(&String::new()),
            ),
        ])
        .await;

    scenario.derive().await;
}

#[tokio::test]
async fn b20_role_lifecycle_updates_state_and_emits_events() {
    let mut scenario = B20TokenScenario::new().await;

    scenario
        .assert_staticcall_cases(vec![
            StaticcallCase::output(
                "DEFAULT_ADMIN_ROLE",
                IB20::DEFAULT_ADMIN_ROLECall {}.abi_encode(),
                B20TokenRole::DefaultAdmin.id().abi_encode(),
            ),
            StaticcallCase::output(
                "MINT_ROLE",
                IB20::MINT_ROLECall {}.abi_encode(),
                B20TokenRole::Mint.id().abi_encode(),
            ),
            StaticcallCase::word(
                "hasRole(default admin, alice)",
                IB20::hasRoleCall {
                    role: B20TokenRole::DefaultAdmin.id(),
                    account: BerylTestEnv::alice(),
                }
                .abi_encode(),
                U256::ONE,
            ),
            StaticcallCase::output(
                "getRoleAdmin(MINT_ROLE)",
                IB20::getRoleAdminCall { role: B20TokenRole::Mint.id() }.abi_encode(),
                B20TokenRole::DefaultAdmin.id().abi_encode(),
            ),
        ])
        .await;

    let grant_bob = scenario.call_tx(IB20::grantRoleCall {
        role: B20TokenRole::Mint.id(),
        account: BerylTestEnv::bob(),
    });
    let block = scenario.build_block_with_transactions(vec![grant_bob]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "grantRole must succeed");
    scenario.assert_log(
        &block,
        0,
        IB20::RoleGranted {
            role: B20TokenRole::Mint.id(),
            account: BerylTestEnv::bob(),
            sender: BerylTestEnv::alice(),
        }
        .encode_log_data(),
    );
    scenario
        .assert_staticcall_cases(vec![StaticcallCase::word(
            "hasRole(MINT_ROLE, bob)",
            IB20::hasRoleCall { role: B20TokenRole::Mint.id(), account: BerylTestEnv::bob() }
                .abi_encode(),
            U256::ONE,
        )])
        .await;

    let revoke_bob = scenario.call_tx(IB20::revokeRoleCall {
        role: B20TokenRole::Mint.id(),
        account: BerylTestEnv::bob(),
    });
    let block = scenario.build_block_with_transactions(vec![revoke_bob]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "revokeRole must succeed");
    scenario.assert_log(
        &block,
        0,
        IB20::RoleRevoked {
            role: B20TokenRole::Mint.id(),
            account: BerylTestEnv::bob(),
            sender: BerylTestEnv::alice(),
        }
        .encode_log_data(),
    );

    let grant_alice_mint = scenario.call_tx(IB20::grantRoleCall {
        role: B20TokenRole::Mint.id(),
        account: BerylTestEnv::alice(),
    });
    let renounce_alice_mint = scenario.call_tx(IB20::renounceRoleCall {
        role: B20TokenRole::Mint.id(),
        callerConfirmation: BerylTestEnv::alice(),
    });
    let set_pause_admin = scenario.call_tx(IB20::setRoleAdminCall {
        role: B20TokenRole::Pause.id(),
        newAdminRole: B20TokenRole::Mint.id(),
    });
    let renounce_last_admin = scenario.call_tx(IB20::renounceLastAdminCall {});
    let block = scenario
        .build_block_with_transactions(vec![
            grant_alice_mint,
            renounce_alice_mint,
            set_pause_admin,
            renounce_last_admin,
        ])
        .await;
    for index in 0..4 {
        assert!(
            scenario.env.user_tx_succeeded(&block, index),
            "role lifecycle tx {index} must succeed"
        );
    }
    scenario.assert_log(
        &block,
        2,
        IB20::RoleAdminChanged {
            role: B20TokenRole::Pause.id(),
            previousAdminRole: B20TokenRole::DefaultAdmin.id(),
            newAdminRole: B20TokenRole::Mint.id(),
        }
        .encode_log_data(),
    );
    scenario.assert_log(
        &block,
        3,
        IB20::LastAdminRenounced { previousAdmin: BerylTestEnv::alice() }.encode_log_data(),
    );
    scenario
        .assert_staticcall_cases(vec![
            StaticcallCase::word(
                "hasRole(MINT_ROLE, bob) after revoke",
                IB20::hasRoleCall { role: B20TokenRole::Mint.id(), account: BerylTestEnv::bob() }
                    .abi_encode(),
                U256::ZERO,
            ),
            StaticcallCase::word(
                "hasRole(MINT_ROLE, alice) after renounce",
                IB20::hasRoleCall { role: B20TokenRole::Mint.id(), account: BerylTestEnv::alice() }
                    .abi_encode(),
                U256::ZERO,
            ),
            StaticcallCase::word(
                "hasRole(DEFAULT_ADMIN_ROLE, alice) after renounceLastAdmin",
                IB20::hasRoleCall {
                    role: B20TokenRole::DefaultAdmin.id(),
                    account: BerylTestEnv::alice(),
                }
                .abi_encode(),
                U256::ZERO,
            ),
            StaticcallCase::output(
                "getRoleAdmin(PAUSE_ROLE) after setRoleAdmin",
                IB20::getRoleAdminCall { role: B20TokenRole::Pause.id() }.abi_encode(),
                B20TokenRole::Mint.id().abi_encode(),
            ),
        ])
        .await;

    scenario.derive().await;
}

#[tokio::test]
async fn b20_extended_mutations_update_state_and_emit_events() {
    let mut scenario = B20TokenScenario::new().await;
    let initial = BerylTestEnv::B20_INITIAL_SUPPLY;
    let new_cap = U256::from(initial + 1_000);
    let grant_roles = [
        B20TokenRole::Metadata,
        B20TokenRole::Mint,
        B20TokenRole::Burn,
        B20TokenRole::Pause,
        B20TokenRole::Unpause,
    ]
    .into_iter()
    .map(|role| {
        scenario.call_tx(IB20::grantRoleCall { role: role.id(), account: BerylTestEnv::alice() })
    })
    .collect();
    let block = scenario.build_block_with_transactions(grant_roles).await;

    for index in 0..5 {
        assert!(scenario.env.user_tx_succeeded(&block, index), "role grant {index} must succeed");
    }

    let transfer_with_memo = scenario.call_tx(IB20::transferWithMemoCall {
        to: BerylTestEnv::bob(),
        amount: U256::from(10),
        memo: MEMO_TRANSFER,
    });
    let approve_bob = scenario
        .call_tx(IB20::approveCall { spender: BerylTestEnv::bob(), amount: U256::from(50) });
    let transfer_from_with_memo = scenario.bob_call_tx(IB20::transferFromWithMemoCall {
        from: BerylTestEnv::alice(),
        to: BerylTestEnv::carol(),
        amount: U256::from(5),
        memo: MEMO_TRANSFER_FROM,
    });
    let update_supply_cap = scenario.call_tx(IB20::updateSupplyCapCall { newSupplyCap: new_cap });
    let update_name =
        scenario.call_tx(IB20::updateNameCall { newName: "Action B20 Updated".to_string() });
    let update_symbol = scenario.call_tx(IB20::updateSymbolCall { newSymbol: "AB20U".to_string() });
    let update_contract_uri =
        scenario.call_tx(IB20::updateContractURICall { newURI: "ipfs://action".to_string() });
    let mint =
        scenario.call_tx(IB20::mintCall { to: BerylTestEnv::alice(), amount: U256::from(20) });
    let mint_with_memo = scenario.call_tx(IB20::mintWithMemoCall {
        to: BerylTestEnv::bob(),
        amount: U256::from(30),
        memo: MEMO_MINT,
    });
    let burn = scenario.call_tx(IB20::burnCall { amount: U256::from(2) });
    let burn_with_memo =
        scenario.call_tx(IB20::burnWithMemoCall { amount: U256::from(3), memo: MEMO_BURN });
    let pause =
        scenario.call_tx(IB20::pauseCall { features: vec![IB20::PausableFeature::TRANSFER] });
    let unpause =
        scenario.call_tx(IB20::unpauseCall { features: vec![IB20::PausableFeature::TRANSFER] });

    let block = scenario
        .build_block_with_transactions(vec![
            transfer_with_memo,
            approve_bob,
            transfer_from_with_memo,
            update_supply_cap,
            update_name,
            update_symbol,
            update_contract_uri,
            mint,
            mint_with_memo,
            burn,
            burn_with_memo,
            pause,
            unpause,
        ])
        .await;

    for index in 0..13 {
        assert!(
            scenario.env.user_tx_succeeded(&block, index),
            "B-20 mutation {index} must succeed"
        );
    }

    scenario.assert_log(
        &block,
        0,
        IB20::Memo { caller: BerylTestEnv::alice(), memo: MEMO_TRANSFER }.encode_log_data(),
    );
    scenario.assert_log(
        &block,
        2,
        IB20::Memo { caller: BerylTestEnv::bob(), memo: MEMO_TRANSFER_FROM }.encode_log_data(),
    );
    scenario.assert_log(
        &block,
        3,
        IB20::SupplyCapUpdated {
            updater: BerylTestEnv::alice(),
            oldSupplyCap: B20_MAX_SUPPLY_CAP,
            newSupplyCap: new_cap,
        }
        .encode_log_data(),
    );
    scenario.assert_log(
        &block,
        4,
        IB20::NameUpdated {
            updater: BerylTestEnv::alice(),
            newName: "Action B20 Updated".to_string(),
        }
        .encode_log_data(),
    );
    scenario.assert_log(
        &block,
        5,
        IB20::SymbolUpdated { updater: BerylTestEnv::alice(), newSymbol: "AB20U".to_string() }
            .encode_log_data(),
    );
    scenario.assert_log(&block, 6, IB20::ContractURIUpdated {}.encode_log_data());
    scenario.assert_log(
        &block,
        8,
        IB20::Memo { caller: BerylTestEnv::alice(), memo: MEMO_MINT }.encode_log_data(),
    );
    scenario.assert_log(
        &block,
        10,
        IB20::Memo { caller: BerylTestEnv::alice(), memo: MEMO_BURN }.encode_log_data(),
    );
    scenario.assert_log(
        &block,
        11,
        IB20::Paused {
            updater: BerylTestEnv::alice(),
            features: vec![IB20::PausableFeature::TRANSFER],
        }
        .encode_log_data(),
    );
    scenario.assert_log(
        &block,
        12,
        IB20::Unpaused {
            updater: BerylTestEnv::alice(),
            features: vec![IB20::PausableFeature::TRANSFER],
        }
        .encode_log_data(),
    );

    scenario.assert_total_supply(initial + 20 + 30 - 2 - 3);
    scenario.assert_allowance(BerylTestEnv::alice(), BerylTestEnv::bob(), 45);

    let zero_mint =
        scenario.call_tx(IB20::mintCall { to: BerylTestEnv::alice(), amount: U256::ZERO });
    let zero_burn = scenario.call_tx(IB20::burnCall { amount: U256::ZERO });
    let block = scenario.build_block_with_transactions(vec![zero_mint, zero_burn]).await;

    for index in 0..2 {
        assert!(
            scenario.env.user_tx_succeeded(&block, index),
            "zero-amount B-20 mutation {index} must succeed (zero-amount ops are valid per ERC-20)"
        );
    }
    scenario.assert_total_supply(initial + 20 + 30 - 2 - 3);

    scenario
        .assert_staticcall_cases(vec![
            StaticcallCase::word(
                "pausedFeatures after unpause",
                IB20::pausedFeaturesCall {}.abi_encode(),
                U256::from(32),
            )
            .with_output(IB20::pausedFeaturesCall::abi_encode_returns(
                &Vec::<IB20::PausableFeature>::new(),
            )),
            StaticcallCase::word(
                "supplyCap after update",
                IB20::supplyCapCall {}.abi_encode(),
                new_cap,
            ),
            StaticcallCase::output(
                "name after update",
                IB20::nameCall {}.abi_encode(),
                IB20::nameCall::abi_encode_returns(&"Action B20 Updated".to_string()),
            ),
            StaticcallCase::output(
                "symbol after update",
                IB20::symbolCall {}.abi_encode(),
                IB20::symbolCall::abi_encode_returns(&"AB20U".to_string()),
            ),
            StaticcallCase::output(
                "contractURI after update",
                IB20::contractURICall {}.abi_encode(),
                IB20::contractURICall::abi_encode_returns(&"ipfs://action".to_string()),
            ),
        ])
        .await;

    scenario.derive().await;
}

#[tokio::test]
async fn b20_permit_updates_allowance_and_nonce() {
    let mut scenario = B20TokenScenario::new().await;
    let value = U256::from(123);
    let deadline = U256::MAX;
    let domain_sep =
        domain_separator(scenario.env.chain_id(), scenario.token, BerylTestEnv::B20_NAME);
    let (v, r, s) = sign_permit(
        domain_sep,
        BerylTestEnv::alice(),
        BerylTestEnv::bob(),
        value,
        U256::ZERO,
        deadline,
    );

    let permit = scenario.call_tx(IB20::permitCall {
        owner: BerylTestEnv::alice(),
        spender: BerylTestEnv::bob(),
        value,
        deadline,
        v,
        r,
        s,
    });
    let block = scenario.build_block_with_transactions(vec![permit]).await;

    assert!(scenario.env.user_tx_succeeded(&block, 0), "permit() transaction must succeed");
    scenario.assert_log(
        &block,
        0,
        IB20::Approval {
            owner: BerylTestEnv::alice(),
            spender: BerylTestEnv::bob(),
            amount: value,
        }
        .encode_log_data(),
    );
    scenario.assert_allowance(BerylTestEnv::alice(), BerylTestEnv::bob(), 123);
    scenario
        .assert_staticcall_cases(vec![StaticcallCase::word(
            "nonces after permit",
            IB20::noncesCall { owner: BerylTestEnv::alice() }.abi_encode(),
            U256::ONE,
        )])
        .await;

    scenario.derive().await;
}

#[tokio::test]
async fn b20_permit_rejects_replay_expired_and_wrong_signature() {
    let mut scenario = B20TokenScenario::new().await;
    let value = U256::from(123);
    let deadline = U256::MAX;
    let domain_sep =
        domain_separator(scenario.env.chain_id(), scenario.token, BerylTestEnv::B20_NAME);
    let (v, r, s) = sign_permit(
        domain_sep,
        BerylTestEnv::alice(),
        BerylTestEnv::bob(),
        value,
        U256::ZERO,
        deadline,
    );

    let valid = scenario.call_tx(IB20::permitCall {
        owner: BerylTestEnv::alice(),
        spender: BerylTestEnv::bob(),
        value,
        deadline,
        v,
        r,
        s,
    });
    let block = scenario.build_block_with_transactions(vec![valid]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "initial permit must succeed");
    scenario.assert_allowance(BerylTestEnv::alice(), BerylTestEnv::bob(), 123);

    let replay = scenario.call_tx(IB20::permitCall {
        owner: BerylTestEnv::alice(),
        spender: BerylTestEnv::bob(),
        value,
        deadline,
        v,
        r,
        s,
    });
    let expired_deadline = U256::ZERO;
    let (expired_v, expired_r, expired_s) = sign_permit(
        domain_sep,
        BerylTestEnv::alice(),
        BerylTestEnv::bob(),
        U256::from(456),
        U256::ONE,
        expired_deadline,
    );
    let expired = scenario.call_tx(IB20::permitCall {
        owner: BerylTestEnv::alice(),
        spender: BerylTestEnv::bob(),
        value: U256::from(456),
        deadline: expired_deadline,
        v: expired_v,
        r: expired_r,
        s: expired_s,
    });
    let (wrong_v, wrong_r, wrong_s) = sign_permit(
        domain_sep,
        BerylTestEnv::bob(),
        BerylTestEnv::bob(),
        U256::from(789),
        U256::ONE,
        deadline,
    );
    let wrong_signature = scenario.call_tx(IB20::permitCall {
        owner: BerylTestEnv::alice(),
        spender: BerylTestEnv::bob(),
        value: U256::from(789),
        deadline,
        v: wrong_v,
        r: wrong_r,
        s: wrong_s,
    });
    let block =
        scenario.build_block_with_transactions(vec![replay, expired, wrong_signature]).await;

    for index in 0..3 {
        assert!(
            !scenario.env.user_tx_succeeded(&block, index),
            "invalid permit {index} must revert"
        );
    }
    scenario.assert_allowance(BerylTestEnv::alice(), BerylTestEnv::bob(), 123);
    scenario
        .assert_staticcall_cases(vec![StaticcallCase::word(
            "nonces after invalid permits",
            IB20::noncesCall { owner: BerylTestEnv::alice() }.abi_encode(),
            U256::ONE,
        )])
        .await;

    scenario.derive().await;
}

struct B20TokenScenario {
    env: BerylTestEnv,
    token: Address,
    blocks: Vec<(BaseBlock, u64)>,
}

impl B20TokenScenario {
    async fn new() -> Self {
        let env = BerylTestEnv::new();
        let token = env.b20_token_address();
        let mut scenario = Self { env, token, blocks: Vec::new() };

        scenario.build_block_with_transactions(Vec::new()).await;

        let activate_b20 = scenario.env.activate_feature_tx(BerylTestEnv::b20_asset_feature());
        let block = scenario.build_block_with_transactions(vec![activate_b20]).await;

        assert!(scenario.env.user_tx_succeeded(&block, 0), "B20_ASSET activation must succeed");

        let create = scenario.env.create_b20_token_tx();
        let block = scenario.build_block_with_transactions(vec![create]).await;

        assert!(
            scenario.env.user_tx_succeeded(&block, 0),
            "B-20 creation transaction must succeed"
        );
        assert!(scenario.env.sequencer.has_code(token), "B-20 token code must be deployed");
        scenario.assert_total_supply(BerylTestEnv::B20_INITIAL_SUPPLY);
        scenario.assert_balances(BerylTestEnv::B20_INITIAL_SUPPLY, 0, 0);

        scenario
    }

    async fn build_block_with_transactions(
        &mut self,
        transactions: Vec<BaseTxEnvelope>,
    ) -> BaseBlock {
        let block = self.env.sequencer.build_next_block_with_transactions(transactions).await;
        let block_number = self.blocks.len() as u64 + 1;
        self.blocks.push((block.clone(), block_number));
        block
    }

    fn call_tx(&self, call: impl SolCall) -> BaseTxEnvelope {
        self.env.create_tx(
            TxKind::Call(self.token),
            Bytes::from(call.abi_encode()),
            BerylTestEnv::B20_GAS_LIMIT,
        )
    }

    fn bob_call_tx(&mut self, call: impl SolCall) -> BaseTxEnvelope {
        self.env.create_bob_tx(
            TxKind::Call(self.token),
            Bytes::from(call.abi_encode()),
            BerylTestEnv::B20_GAS_LIMIT,
        )
    }

    async fn assert_staticcall_cases(&mut self, cases: Vec<StaticcallCase>) {
        let mut probes = Vec::with_capacity(cases.len());
        let mut deployments = Vec::with_capacity(cases.len());
        for _ in &cases {
            let (probe, deploy) = self.env.deploy_staticcall_probe_tx(self.token);
            probes.push(probe);
            deployments.push(deploy);
        }

        let deploy_block = self.build_block_with_transactions(deployments).await;
        for index in 0..cases.len() {
            assert!(
                self.env.user_tx_succeeded(&deploy_block, index),
                "staticcall probe deployment {index} must succeed"
            );
        }

        let calls = probes
            .iter()
            .zip(cases.iter())
            .map(|(probe, case)| {
                self.env.call_staticcall_probe_tx(
                    *probe,
                    Bytes::from(case.input.clone()),
                    BerylTestEnv::B20_PROBE_GAS_LIMIT,
                )
            })
            .collect();
        let call_block = self.build_block_with_transactions(calls).await;
        for (index, (probe, case)) in probes.iter().zip(cases.iter()).enumerate() {
            assert!(
                self.env.user_tx_succeeded(&call_block, index),
                "{} probe transaction must succeed",
                case.label
            );
            assert!(
                self.env.probe_call_succeeded(*probe),
                "{} staticcall must succeed",
                case.label
            );
            if let Some(expected) = case.expected_word {
                assert_eq!(
                    self.env.probe_return_word(*probe),
                    expected,
                    "{} staticcall must return the expected first word",
                    case.label
                );
            }
            assert_eq!(
                self.env.probe_return_size(*probe),
                U256::from(case.expected_output.len()),
                "{} staticcall must return the expected byte length",
                case.label
            );
            assert_eq!(
                self.env.probe_return_hash(*probe),
                keccak256(&case.expected_output),
                "{} staticcall must return the expected ABI payload hash",
                case.label
            );
        }
    }

    fn assert_total_supply(&self, total_supply: u64) {
        assert_eq!(
            self.env.b20_total_supply(self.token),
            U256::from(total_supply),
            "B-20 total supply must match expected value"
        );
    }

    fn assert_balances(&self, alice: u64, bob: u64, carol: u64) {
        assert_eq!(
            self.env.b20_balance(self.token, BerylTestEnv::alice()),
            U256::from(alice),
            "Alice B-20 balance must match expected value"
        );
        assert_eq!(
            self.env.b20_balance(self.token, BerylTestEnv::bob()),
            U256::from(bob),
            "Bob B-20 balance must match expected value"
        );
        assert_eq!(
            self.env.b20_balance(self.token, BerylTestEnv::carol()),
            U256::from(carol),
            "Carol B-20 balance must match expected value"
        );
    }

    fn assert_allowance(&self, owner: Address, spender: Address, amount: u64) {
        assert_eq!(
            self.env.b20_allowance(self.token, owner, spender),
            U256::from(amount),
            "B-20 allowance must match expected value"
        );
    }

    fn assert_log(
        &self,
        block: &BaseBlock,
        user_tx_index: usize,
        expected: alloy_primitives::LogData,
    ) {
        assert!(
            self.env
                .user_tx_receipt(block, user_tx_index)
                .logs()
                .iter()
                .any(|log| log.address == self.token && log.data == expected),
            "B-20 transaction {user_tx_index} must emit the expected event"
        );
    }

    fn assert_transfer_log(&self, block: &BaseBlock, from: Address, to: Address, amount: u64) {
        assert!(
            self.env.b20_transfer_log_emitted(block, 0, self.token, from, to, U256::from(amount),),
            "B-20 transfer must emit a Transfer event"
        );
    }

    fn assert_approval_log(
        &self,
        block: &BaseBlock,
        owner: Address,
        spender: Address,
        amount: u64,
    ) {
        assert!(
            self.env.b20_approval_log_emitted(
                block,
                0,
                self.token,
                owner,
                spender,
                U256::from(amount),
            ),
            "B-20 approval must emit an Approval event"
        );
    }

    async fn derive(mut self) {
        let expected_safe_head = self.blocks.len() as u64;
        self.env.derive_blocks(self.blocks, expected_safe_head).await;
    }
}

struct StaticcallCase {
    label: &'static str,
    input: Vec<u8>,
    expected_word: Option<U256>,
    expected_output: Vec<u8>,
}

impl StaticcallCase {
    fn word(label: &'static str, input: Vec<u8>, expected_word: U256) -> Self {
        Self {
            label,
            input,
            expected_word: Some(expected_word),
            expected_output: expected_word.abi_encode(),
        }
    }

    fn output(label: &'static str, input: Vec<u8>, expected_output: Vec<u8>) -> Self {
        let expected_word = expected_output.get(..32).map(U256::from_be_slice);
        Self { label, input, expected_word, expected_output }
    }

    fn with_output(mut self, expected_output: Vec<u8>) -> Self {
        self.expected_word = expected_output.get(..32).map(U256::from_be_slice);
        self.expected_output = expected_output;
        self
    }
}

fn sign_permit(
    domain_sep: B256,
    owner: Address,
    spender: Address,
    value: U256,
    nonce: U256,
    deadline: U256,
) -> (u8, B256, B256) {
    let permit_typehash = keccak256(PERMIT_TYPE);
    let struct_hash =
        keccak256((permit_typehash, owner, spender, value, nonce, deadline).abi_encode());

    let mut digest = [0u8; 66];
    digest[0] = 0x19;
    digest[1] = 0x01;
    digest[2..34].copy_from_slice(domain_sep.as_slice());
    digest[34..66].copy_from_slice(struct_hash.as_slice());
    let hash = keccak256(digest);

    let signer = PrivateKeySigner::from_bytes(&TEST_ACCOUNT_KEY).expect("valid test signer");
    let sig = signer.sign_hash_sync(&hash).expect("permit signing must succeed");
    let r = B256::from(sig.r().to_be_bytes::<32>());
    let s = B256::from(sig.s().to_be_bytes::<32>());
    let v = if sig.v() { 28 } else { 27 };
    (v, r, s)
}

fn domain_separator(chain_id: u64, token: Address, name: &str) -> B256 {
    let domain_typehash = keccak256(DOMAIN_TYPE);
    let name_hash = keccak256(name.as_bytes());
    let version_hash = keccak256(DOMAIN_VERSION);
    keccak256((domain_typehash, name_hash, version_hash, U256::from(chain_id), token).abi_encode())
}

fn domain_separator_word(chain_id: u64, token: Address, name: &str) -> U256 {
    U256::from_be_slice(domain_separator(chain_id, token, name).as_slice())
}

const fn eip712_domain_fields_word() -> U256 {
    let mut word = [0u8; 32];
    word[0] = 0x0f; // bits 0+1+2+3: name + version + chainId + verifyingContract
    U256::from_be_bytes(word)
}

fn eip712_domain_return(chain_id: u64, token: Address, name: &str) -> IB20::eip712DomainReturn {
    IB20::eip712DomainReturn {
        fields: FixedBytes::<1>::from([0x0f]),
        name: name.to_string(),
        version: "1".to_string(),
        chainId: U256::from(chain_id),
        verifyingContract: token,
        salt: B256::ZERO,
        extensions: Vec::new(),
    }
}

struct B20StaticcallProbes {
    total_supply: Address,
    alice_balance: Address,
    bob_balance: Address,
    carol_balance: Address,
    allowance: Address,
    decimals: Address,
}

impl B20StaticcallProbes {
    async fn deploy(scenario: &mut B20TokenScenario) -> Self {
        let (total_supply, deploy_total_supply) =
            scenario.env.deploy_staticcall_probe_tx(scenario.token);
        let (alice_balance, deploy_alice_balance) =
            scenario.env.deploy_staticcall_probe_tx(scenario.token);
        let (bob_balance, deploy_bob_balance) =
            scenario.env.deploy_staticcall_probe_tx(scenario.token);
        let (carol_balance, deploy_carol_balance) =
            scenario.env.deploy_staticcall_probe_tx(scenario.token);
        let (allowance, deploy_allowance) = scenario.env.deploy_staticcall_probe_tx(scenario.token);
        let (decimals, deploy_decimals) = scenario.env.deploy_staticcall_probe_tx(scenario.token);

        let block = scenario
            .build_block_with_transactions(vec![
                deploy_total_supply,
                deploy_alice_balance,
                deploy_bob_balance,
                deploy_carol_balance,
                deploy_allowance,
                deploy_decimals,
            ])
            .await;
        for index in 0..6 {
            assert!(
                scenario.env.user_tx_succeeded(&block, index),
                "B-20 staticcall probe deployment transaction {index} must succeed"
            );
        }

        Self { total_supply, alice_balance, bob_balance, carol_balance, allowance, decimals }
    }

    fn call_txs(&self, scenario: &B20TokenScenario) -> Vec<BaseTxEnvelope> {
        vec![
            scenario.env.probe_b20_total_supply_tx(self.total_supply),
            scenario.env.probe_b20_balance_tx(self.alice_balance, BerylTestEnv::alice()),
            scenario.env.probe_b20_balance_tx(self.bob_balance, BerylTestEnv::bob()),
            scenario.env.probe_b20_balance_tx(self.carol_balance, BerylTestEnv::carol()),
            scenario.env.probe_b20_allowance_tx(
                self.allowance,
                BerylTestEnv::alice(),
                BerylTestEnv::bob(),
            ),
            scenario.env.probe_b20_decimals_tx(self.decimals),
        ]
    }

    fn assert_returns(&self, scenario: &B20TokenScenario, expected: B20ProbeExpectations) {
        Self::assert_probe_return(scenario, self.total_supply, expected.total_supply);
        Self::assert_probe_return(scenario, self.alice_balance, expected.alice_balance);
        Self::assert_probe_return(scenario, self.bob_balance, expected.bob_balance);
        Self::assert_probe_return(scenario, self.carol_balance, expected.carol_balance);
        Self::assert_probe_return(scenario, self.allowance, expected.allowance);
        Self::assert_probe_return(scenario, self.decimals, u64::from(expected.decimals));
    }

    fn assert_probe_return(scenario: &B20TokenScenario, probe: Address, expected: u64) {
        let expected_output = U256::from(expected).abi_encode();
        assert!(scenario.env.probe_call_succeeded(probe), "B-20 staticcall probe must succeed");
        assert_eq!(
            scenario.env.probe_return_word(probe),
            U256::from(expected),
            "B-20 staticcall probe must return the expected word"
        );
        assert_eq!(
            scenario.env.probe_return_size(probe),
            U256::from(expected_output.len()),
            "B-20 staticcall probe must return one ABI word"
        );
        assert_eq!(
            scenario.env.probe_return_hash(probe),
            keccak256(&expected_output),
            "B-20 staticcall probe must return the expected ABI payload hash"
        );
    }
}

struct B20ProbeExpectations {
    total_supply: u64,
    alice_balance: u64,
    bob_balance: u64,
    carol_balance: u64,
    allowance: u64,
    decimals: u8,
}
