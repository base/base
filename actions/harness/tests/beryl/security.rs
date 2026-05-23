//! Security B-20 precompile action tests across the Base Beryl boundary.

use alloy_consensus::TxReceipt;
use alloy_primitives::{Address, B256, Bytes, LogData, TxKind, U256, keccak256};
use alloy_sol_types::{SolCall, SolEvent, SolValue};
use base_common_consensus::{BaseBlock, BaseTxEnvelope};
use base_common_precompiles::{
    B20FactoryStorage, B20TokenRole, IB20, IB20Factory, IB20Security, PolicyRegistryStorage,
    REDEEM_SENDER_POLICY,
};

use crate::{
    env::BerylTestEnv,
    test_helpers::{self, StaticcallCase, word_from_address},
};

const WAD: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);
const UPDATED_RATIO: U256 = U256::from_limbs([2_000_000_000_000_000_000, 0, 0, 0]);
const UPDATED_MINIMUM_REDEEMABLE: U256 = U256::from_limbs([20, 0, 0, 0]);
const BOB_MINT_AMOUNT: u64 = 100;
const CAROL_MINT_AMOUNT: u64 = 200;
const BOB_BURN_AMOUNT: u64 = 10;
const CAROL_BURN_AMOUNT: u64 = 20;
const REDEEM_AMOUNT: u64 = 20;
const REDEEM_WITH_MEMO_AMOUNT: u64 = 30;
const REDEEM_MEMO: B256 = B256::repeat_byte(0x61);
const CUSIP: &str = "123456789";
const FIGI: &str = "BBG000000001";
const ANNOUNCEMENT_ID: &str = "security-action-1";
const ANNOUNCEMENT_DESCRIPTION: &str = "update FIGI";
const ANNOUNCEMENT_URI: &str = "ipfs://security-action";

#[tokio::test]
async fn security_creation_initializes_identifiers_and_factory_views() {
    let mut scenario = B20SecurityScenario::new().await;

    scenario
        .assert_staticcall_cases(
            B20FactoryStorage::ADDRESS,
            vec![
                StaticcallCase::word(
                    "factory getB20Address(SECURITY)",
                    IB20Factory::getB20AddressCall {
                        variant: IB20Factory::B20Variant::SECURITY,
                        sender: BerylTestEnv::alice(),
                        salt: BerylTestEnv::b20_security_salt(),
                    }
                    .abi_encode(),
                    word_from_address(scenario.token),
                ),
                StaticcallCase::word(
                    "factory isB20(security)",
                    IB20Factory::isB20Call { token: scenario.token }.abi_encode(),
                    U256::ONE,
                ),
                StaticcallCase::word(
                    "factory isB20Initialized(security)",
                    IB20Factory::isB20InitializedCall { token: scenario.token }.abi_encode(),
                    U256::ONE,
                ),
            ],
        )
        .await;

    scenario
        .assert_staticcall_cases(
            scenario.token,
            vec![
                StaticcallCase::string(
                    "name",
                    IB20::nameCall {}.abi_encode(),
                    BerylTestEnv::B20_SECURITY_NAME,
                ),
                StaticcallCase::string(
                    "symbol",
                    IB20::symbolCall {}.abi_encode(),
                    BerylTestEnv::B20_SECURITY_SYMBOL,
                ),
                StaticcallCase::string("contractURI", IB20::contractURICall {}.abi_encode(), ""),
                StaticcallCase::string(
                    "securityIdentifier(ISIN)",
                    IB20Security::securityIdentifierCall { identifierType: "ISIN".to_string() }
                        .abi_encode(),
                    BerylTestEnv::B20_SECURITY_ISIN,
                ),
                StaticcallCase::word(
                    "decimals",
                    IB20::decimalsCall {}.abi_encode(),
                    U256::from(BerylTestEnv::B20_SECURITY_DECIMALS),
                ),
                StaticcallCase::word(
                    "totalSupply",
                    IB20::totalSupplyCall {}.abi_encode(),
                    U256::from(BerylTestEnv::B20_INITIAL_SUPPLY),
                ),
                StaticcallCase::word(
                    "balanceOf(alice)",
                    IB20::balanceOfCall { account: BerylTestEnv::alice() }.abi_encode(),
                    U256::from(BerylTestEnv::B20_INITIAL_SUPPLY),
                ),
                StaticcallCase::word(
                    "sharesToTokensRatio",
                    IB20Security::sharesToTokensRatioCall {}.abi_encode(),
                    WAD,
                ),
                StaticcallCase::word(
                    "WAD_PRECISION",
                    IB20Security::WAD_PRECISIONCall {}.abi_encode(),
                    WAD,
                ),
                StaticcallCase::word(
                    "toShares",
                    IB20Security::toSharesCall { balance: U256::from(100) }.abi_encode(),
                    U256::from(100),
                ),
                StaticcallCase::word(
                    "sharesOf(alice)",
                    IB20Security::sharesOfCall { account: BerylTestEnv::alice() }.abi_encode(),
                    U256::from(BerylTestEnv::B20_INITIAL_SUPPLY),
                ),
                StaticcallCase::word(
                    "minimumRedeemable",
                    IB20Security::minimumRedeemableCall {}.abi_encode(),
                    U256::from(BerylTestEnv::B20_SECURITY_MINIMUM_REDEEMABLE),
                ),
                StaticcallCase::word(
                    "isAnnouncementIdUsed(fresh)",
                    IB20Security::isAnnouncementIdUsedCall { id: ANNOUNCEMENT_ID.to_string() }
                        .abi_encode(),
                    U256::ZERO,
                ),
                StaticcallCase::bytes32(
                    "SECURITY_OPERATOR_ROLE",
                    IB20Security::SECURITY_OPERATOR_ROLECall {}.abi_encode(),
                    security_operator_role(),
                ),
                StaticcallCase::bytes32(
                    "BURN_FROM_ROLE",
                    IB20Security::BURN_FROM_ROLECall {}.abi_encode(),
                    burn_from_role(),
                ),
                StaticcallCase::bytes32(
                    "REDEEM_SENDER_POLICY",
                    IB20Security::REDEEM_SENDER_POLICYCall {}.abi_encode(),
                    REDEEM_SENDER_POLICY,
                ),
                StaticcallCase::word(
                    "policyId(REDEEM_SENDER_POLICY)",
                    IB20::policyIdCall { policyScope: REDEEM_SENDER_POLICY }.abi_encode(),
                    U256::from(PolicyRegistryStorage::ALWAYS_ALLOW_ID),
                ),
                StaticcallCase::returndata(
                    "pausedFeatures",
                    IB20::pausedFeaturesCall {}.abi_encode(),
                    Vec::<IB20::PausableFeature>::new().abi_encode(),
                ),
            ],
        )
        .await;

    scenario.derive().await;
}

#[tokio::test]
async fn security_mutations_update_state_and_emit_events() {
    let mut scenario = B20SecurityScenario::new().await;
    scenario
        .grant_roles([security_operator_role(), burn_from_role(), B20TokenRole::Mint.id()])
        .await;

    let update_ratio = scenario
        .call_tx(IB20Security::updateShareRatioCall { newSharesToTokensRatio: UPDATED_RATIO });
    let update_minimum = scenario.call_tx(IB20Security::updateMinimumRedeemableCall {
        newMinimumRedeemable: UPDATED_MINIMUM_REDEEMABLE,
    });
    let update_cusip = scenario.call_tx(IB20Security::updateSecurityIdentifierCall {
        identifierType: "CUSIP".to_string(),
        value: CUSIP.to_string(),
    });
    let batch_mint = scenario.call_tx(IB20Security::batchMintCall {
        recipients: vec![BerylTestEnv::bob(), BerylTestEnv::carol()],
        amounts: vec![U256::from(BOB_MINT_AMOUNT), U256::from(CAROL_MINT_AMOUNT)],
    });
    let batch_burn = scenario.call_tx(IB20Security::batchBurnCall {
        accounts: vec![BerylTestEnv::bob(), BerylTestEnv::carol()],
        amounts: vec![U256::from(BOB_BURN_AMOUNT), U256::from(CAROL_BURN_AMOUNT)],
    });
    let redeem = scenario.call_tx(IB20Security::redeemCall { amount: U256::from(REDEEM_AMOUNT) });
    let redeem_with_memo = scenario.call_tx(IB20Security::redeemWithMemoCall {
        amount: U256::from(REDEEM_WITH_MEMO_AMOUNT),
        memo: REDEEM_MEMO,
    });
    let announced_identifier = IB20Security::updateSecurityIdentifierCall {
        identifierType: "FIGI".to_string(),
        value: FIGI.to_string(),
    };
    let announce = scenario.call_tx(IB20Security::announceCall {
        internalCalls: vec![Bytes::from(announced_identifier.abi_encode())],
        id: ANNOUNCEMENT_ID.to_string(),
        description: ANNOUNCEMENT_DESCRIPTION.to_string(),
        uri: ANNOUNCEMENT_URI.to_string(),
    });
    let block = scenario
        .build_block_with_transactions(vec![
            update_ratio,
            update_minimum,
            update_cusip,
            batch_mint,
            batch_burn,
            redeem,
            redeem_with_memo,
            announce,
        ])
        .await;

    for index in 0..8 {
        assert!(
            scenario.env.user_tx_succeeded(&block, index),
            "security mutation {index} must succeed"
        );
    }

    scenario.assert_log(
        &block,
        0,
        IB20Security::ShareRatioUpdated { sharesToTokensRatio: UPDATED_RATIO }.encode_log_data(),
    );
    scenario.assert_log(
        &block,
        1,
        IB20Security::MinimumRedeemableUpdated {
            caller: BerylTestEnv::alice(),
            newMinimumRedeemable: UPDATED_MINIMUM_REDEEMABLE,
        }
        .encode_log_data(),
    );
    scenario.assert_log(
        &block,
        2,
        IB20Security::SecurityIdentifierUpdated {
            identifierType: "CUSIP".to_string(),
            value: CUSIP.to_string(),
        }
        .encode_log_data(),
    );
    scenario.assert_log(
        &block,
        3,
        IB20::Transfer {
            from: Address::ZERO,
            to: BerylTestEnv::bob(),
            amount: U256::from(BOB_MINT_AMOUNT),
        }
        .encode_log_data(),
    );
    scenario.assert_log(
        &block,
        3,
        IB20::Transfer {
            from: Address::ZERO,
            to: BerylTestEnv::carol(),
            amount: U256::from(CAROL_MINT_AMOUNT),
        }
        .encode_log_data(),
    );
    scenario.assert_log(
        &block,
        4,
        IB20::Transfer {
            from: BerylTestEnv::bob(),
            to: Address::ZERO,
            amount: U256::from(BOB_BURN_AMOUNT),
        }
        .encode_log_data(),
    );
    scenario.assert_log(
        &block,
        4,
        IB20::Transfer {
            from: BerylTestEnv::carol(),
            to: Address::ZERO,
            amount: U256::from(CAROL_BURN_AMOUNT),
        }
        .encode_log_data(),
    );
    scenario.assert_log(
        &block,
        5,
        IB20Security::Redeemed {
            from: BerylTestEnv::alice(),
            amt: U256::from(REDEEM_AMOUNT),
            sharesToTokensRatio: UPDATED_RATIO,
        }
        .encode_log_data(),
    );
    scenario.assert_log(
        &block,
        6,
        IB20::Memo { caller: BerylTestEnv::alice(), memo: REDEEM_MEMO }.encode_log_data(),
    );
    scenario.assert_log(
        &block,
        6,
        IB20Security::Redeemed {
            from: BerylTestEnv::alice(),
            amt: U256::from(REDEEM_WITH_MEMO_AMOUNT),
            sharesToTokensRatio: UPDATED_RATIO,
        }
        .encode_log_data(),
    );
    scenario.assert_log(
        &block,
        7,
        IB20Security::Announcement {
            caller: BerylTestEnv::alice(),
            id: ANNOUNCEMENT_ID.to_string(),
            description: ANNOUNCEMENT_DESCRIPTION.to_string(),
            uri: ANNOUNCEMENT_URI.to_string(),
        }
        .encode_log_data(),
    );
    scenario.assert_log(
        &block,
        7,
        IB20Security::SecurityIdentifierUpdated {
            identifierType: "FIGI".to_string(),
            value: FIGI.to_string(),
        }
        .encode_log_data(),
    );
    scenario.assert_log(
        &block,
        7,
        IB20Security::EndAnnouncement { id: ANNOUNCEMENT_ID.to_string() }.encode_log_data(),
    );

    scenario.assert_total_supply(
        BerylTestEnv::B20_INITIAL_SUPPLY + BOB_MINT_AMOUNT + CAROL_MINT_AMOUNT
            - BOB_BURN_AMOUNT
            - CAROL_BURN_AMOUNT
            - REDEEM_AMOUNT
            - REDEEM_WITH_MEMO_AMOUNT,
    );
    scenario.assert_balances(
        BerylTestEnv::B20_INITIAL_SUPPLY - REDEEM_AMOUNT - REDEEM_WITH_MEMO_AMOUNT,
        BOB_MINT_AMOUNT - BOB_BURN_AMOUNT,
        CAROL_MINT_AMOUNT - CAROL_BURN_AMOUNT,
    );

    scenario
        .assert_staticcall_cases(
            scenario.token,
            vec![
                StaticcallCase::word(
                    "sharesToTokensRatio after update",
                    IB20Security::sharesToTokensRatioCall {}.abi_encode(),
                    UPDATED_RATIO,
                ),
                StaticcallCase::word(
                    "toShares after update",
                    IB20Security::toSharesCall { balance: U256::from(50) }.abi_encode(),
                    U256::from(100),
                ),
                StaticcallCase::word(
                    "sharesOf(alice) after redeem",
                    IB20Security::sharesOfCall { account: BerylTestEnv::alice() }.abi_encode(),
                    U256::from(BerylTestEnv::B20_INITIAL_SUPPLY - 50) * U256::from(2),
                ),
                StaticcallCase::word(
                    "minimumRedeemable after update",
                    IB20Security::minimumRedeemableCall {}.abi_encode(),
                    UPDATED_MINIMUM_REDEEMABLE,
                ),
                StaticcallCase::string(
                    "securityIdentifier(CUSIP)",
                    IB20Security::securityIdentifierCall { identifierType: "CUSIP".to_string() }
                        .abi_encode(),
                    CUSIP,
                ),
                StaticcallCase::string(
                    "securityIdentifier(FIGI)",
                    IB20Security::securityIdentifierCall { identifierType: "FIGI".to_string() }
                        .abi_encode(),
                    FIGI,
                ),
                StaticcallCase::word(
                    "isAnnouncementIdUsed",
                    IB20Security::isAnnouncementIdUsedCall { id: ANNOUNCEMENT_ID.to_string() }
                        .abi_encode(),
                    U256::ONE,
                ),
                StaticcallCase::word(
                    "totalSupply after security mutations",
                    IB20::totalSupplyCall {}.abi_encode(),
                    U256::from(1_000_220),
                ),
            ],
        )
        .await;

    scenario.derive().await;
}

#[tokio::test]
async fn security_mutations_revert_on_invalid_inputs() {
    let mut scenario = B20SecurityScenario::new().await;
    scenario
        .grant_roles([security_operator_role(), burn_from_role(), B20TokenRole::Mint.id()])
        .await;

    let first_announcement = scenario.call_tx(IB20Security::announceCall {
        internalCalls: Vec::new(),
        id: "duplicate-id".to_string(),
        description: "initial".to_string(),
        uri: "ipfs://initial".to_string(),
    });
    let empty_batch_mint = scenario
        .call_tx(IB20Security::batchMintCall { recipients: Vec::new(), amounts: Vec::new() });
    let mismatched_batch_mint = scenario.call_tx(IB20Security::batchMintCall {
        recipients: vec![BerylTestEnv::bob()],
        amounts: vec![U256::from(1), U256::from(2)],
    });
    let mismatched_batch_burn = scenario.call_tx(IB20Security::batchBurnCall {
        accounts: vec![BerylTestEnv::bob()],
        amounts: vec![U256::from(1), U256::from(2)],
    });
    let below_minimum_redeem = scenario.call_tx(IB20Security::redeemCall { amount: U256::from(1) });
    let empty_identifier_type = scenario.call_tx(IB20Security::updateSecurityIdentifierCall {
        identifierType: String::new(),
        value: "x".to_string(),
    });
    let duplicate_announcement = scenario.call_tx(IB20Security::announceCall {
        internalCalls: Vec::new(),
        id: "duplicate-id".to_string(),
        description: "again".to_string(),
        uri: "ipfs://again".to_string(),
    });
    let malformed_internal_call = scenario.call_tx(IB20Security::announceCall {
        internalCalls: vec![Bytes::from(vec![1, 2, 3])],
        id: "malformed-id".to_string(),
        description: "malformed".to_string(),
        uri: "ipfs://malformed".to_string(),
    });
    let recursive_call = IB20Security::announceCall {
        internalCalls: Vec::new(),
        id: "inner".to_string(),
        description: "inner".to_string(),
        uri: "ipfs://inner".to_string(),
    };
    let recursive_announcement = scenario.call_tx(IB20Security::announceCall {
        internalCalls: vec![Bytes::from(recursive_call.abi_encode())],
        id: "recursive-id".to_string(),
        description: "recursive".to_string(),
        uri: "ipfs://recursive".to_string(),
    });
    let block = scenario
        .build_block_with_transactions(vec![
            first_announcement,
            empty_batch_mint,
            mismatched_batch_mint,
            mismatched_batch_burn,
            below_minimum_redeem,
            empty_identifier_type,
            duplicate_announcement,
            malformed_internal_call,
            recursive_announcement,
        ])
        .await;

    assert!(scenario.env.user_tx_succeeded(&block, 0), "first announce() must succeed");
    for index in 1..9 {
        assert!(
            !scenario.env.user_tx_succeeded(&block, index),
            "invalid security mutation {index} must revert"
        );
    }
    scenario.assert_total_supply(BerylTestEnv::B20_INITIAL_SUPPLY);
    scenario.assert_balances(BerylTestEnv::B20_INITIAL_SUPPLY, 0, 0);

    scenario
        .assert_staticcall_cases(
            scenario.token,
            vec![
                StaticcallCase::word(
                    "duplicate announcement id remains used",
                    IB20Security::isAnnouncementIdUsedCall { id: "duplicate-id".to_string() }
                        .abi_encode(),
                    U256::ONE,
                ),
                StaticcallCase::word(
                    "failed malformed announcement id is rolled back",
                    IB20Security::isAnnouncementIdUsedCall { id: "malformed-id".to_string() }
                        .abi_encode(),
                    U256::ZERO,
                ),
                StaticcallCase::word(
                    "failed recursive announcement id is rolled back",
                    IB20Security::isAnnouncementIdUsedCall { id: "recursive-id".to_string() }
                        .abi_encode(),
                    U256::ZERO,
                ),
            ],
        )
        .await;

    scenario.derive().await;
}

#[tokio::test]
async fn security_calls_revert_while_security_feature_is_deactivated() {
    let mut scenario = B20SecurityScenario::new().await;

    let deactivate_security =
        scenario.env.deactivate_feature_tx(BerylTestEnv::b20_security_feature());
    let block = scenario.build_block_with_transactions(vec![deactivate_security]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "B20_SECURITY deactivation must succeed");

    let transfer_while_deactivated =
        scenario.call_tx(IB20::transferCall { to: BerylTestEnv::bob(), amount: U256::from(1) });
    let block = scenario.build_block_with_transactions(vec![transfer_while_deactivated]).await;
    assert!(
        !scenario.env.user_tx_succeeded(&block, 0),
        "security token call must revert while B20_SECURITY is deactivated"
    );

    let (probe, deploy_probe) = scenario.env.deploy_staticcall_probe_tx(scenario.token);
    let block = scenario.build_block_with_transactions(vec![deploy_probe]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "staticcall probe must deploy");

    let probe_call = scenario.env.call_staticcall_probe_tx(
        probe,
        Bytes::from(IB20Security::sharesToTokensRatioCall {}.abi_encode()),
        BerylTestEnv::B20_PROBE_GAS_LIMIT,
    );
    let block = scenario.build_block_with_transactions(vec![probe_call]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "probe transaction must succeed");
    assert!(
        !scenario.env.probe_call_succeeded(probe),
        "security staticcall must fail while B20_SECURITY is deactivated"
    );

    let reactivate_security =
        scenario.env.activate_feature_tx(BerylTestEnv::b20_security_feature());
    let block = scenario.build_block_with_transactions(vec![reactivate_security]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "B20_SECURITY re-activation must succeed");

    let transfer_after_reactivate =
        scenario.call_tx(IB20::transferCall { to: BerylTestEnv::bob(), amount: U256::from(1) });
    let block = scenario.build_block_with_transactions(vec![transfer_after_reactivate]).await;
    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "security token call must succeed after B20_SECURITY is re-activated"
    );
    scenario.assert_balances(BerylTestEnv::B20_INITIAL_SUPPLY - 1, 1, 0);

    scenario.derive().await;
}

struct B20SecurityScenario {
    env: BerylTestEnv,
    token: Address,
    blocks: Vec<(BaseBlock, u64)>,
}

impl B20SecurityScenario {
    async fn new() -> Self {
        let env = BerylTestEnv::new();
        let token = env.b20_security_address();
        let mut scenario = Self { env, token, blocks: Vec::new() };

        scenario.build_block_with_transactions(Vec::new()).await;

        let activate_factory =
            scenario.env.activate_feature_tx(BerylTestEnv::b20_factory_feature());
        let activate_security =
            scenario.env.activate_feature_tx(BerylTestEnv::b20_security_feature());
        let activate_policy =
            scenario.env.activate_feature_tx(BerylTestEnv::policy_registry_feature());
        let block = scenario
            .build_block_with_transactions(vec![
                activate_factory,
                activate_security,
                activate_policy,
            ])
            .await;

        assert!(scenario.env.user_tx_succeeded(&block, 0), "TOKEN_FACTORY activation must succeed");
        assert!(scenario.env.user_tx_succeeded(&block, 1), "B20_SECURITY activation must succeed");
        assert!(
            scenario.env.user_tx_succeeded(&block, 2),
            "POLICY_REGISTRY activation must succeed"
        );

        let create = scenario.env.create_b20_security_tx();
        let block = scenario.build_block_with_transactions(vec![create]).await;

        assert!(
            scenario.env.user_tx_succeeded(&block, 0),
            "security B-20 creation transaction must succeed"
        );
        assert!(scenario.env.sequencer.has_code(token), "security B-20 code must be deployed");
        scenario.assert_token_created_log(&block);
        scenario.assert_log(
            &block,
            0,
            IB20::Transfer {
                from: Address::ZERO,
                to: BerylTestEnv::alice(),
                amount: U256::from(BerylTestEnv::B20_INITIAL_SUPPLY),
            }
            .encode_log_data(),
        );
        scenario.assert_total_supply(BerylTestEnv::B20_INITIAL_SUPPLY);
        scenario.assert_balances(BerylTestEnv::B20_INITIAL_SUPPLY, 0, 0);

        scenario
    }

    async fn build_block_with_transactions(
        &mut self,
        transactions: Vec<BaseTxEnvelope>,
    ) -> BaseBlock {
        test_helpers::build_block_with_transactions(&mut self.env, &mut self.blocks, transactions)
            .await
    }

    async fn grant_roles(&mut self, roles: impl IntoIterator<Item = B256>) {
        let grants = roles
            .into_iter()
            .map(|role| self.call_tx(IB20::grantRoleCall { role, account: BerylTestEnv::alice() }))
            .collect::<Vec<_>>();
        let grant_count = grants.len();
        let block = self.build_block_with_transactions(grants).await;
        for index in 0..grant_count {
            assert!(self.env.user_tx_succeeded(&block, index), "role grant {index} must succeed");
        }
    }

    fn call_tx(&self, call: impl SolCall) -> BaseTxEnvelope {
        self.env.create_tx(
            TxKind::Call(self.token),
            Bytes::from(call.abi_encode()),
            BerylTestEnv::B20_GAS_LIMIT,
        )
    }

    async fn assert_staticcall_cases(&mut self, target: Address, cases: Vec<StaticcallCase>) {
        test_helpers::assert_staticcall_cases(
            &mut self.env,
            &mut self.blocks,
            target,
            cases,
            "security",
        )
        .await;
    }

    fn assert_total_supply(&self, total_supply: u64) {
        test_helpers::assert_total_supply(&self.env, self.token, "security B-20", total_supply);
    }

    fn assert_balances(&self, alice: u64, bob: u64, carol: u64) {
        test_helpers::assert_balances(&self.env, self.token, "security B-20", alice, bob, carol);
    }

    fn assert_token_created_log(&self, block: &BaseBlock) {
        let expected = IB20Factory::B20Created {
            token: self.token,
            variant: IB20Factory::B20Variant::SECURITY,
            name: BerylTestEnv::B20_SECURITY_NAME.to_string(),
            symbol: BerylTestEnv::B20_SECURITY_SYMBOL.to_string(),
            decimals: BerylTestEnv::B20_SECURITY_DECIMALS,
        }
        .encode_log_data();
        self.assert_receipt_log(block, 0, B20FactoryStorage::ADDRESS, expected);
    }

    fn assert_log(&self, block: &BaseBlock, user_tx_index: usize, expected: LogData) {
        self.assert_receipt_log(block, user_tx_index, self.token, expected);
    }

    fn assert_receipt_log(
        &self,
        block: &BaseBlock,
        user_tx_index: usize,
        address: Address,
        expected: LogData,
    ) {
        assert!(
            self.env
                .user_tx_receipt(block, user_tx_index)
                .logs()
                .iter()
                .any(|log| log.address == address && log.data == expected),
            "security B-20 transaction {user_tx_index} must emit the expected event"
        );
    }

    async fn derive(mut self) {
        let expected_safe_head = self.blocks.len() as u64;
        self.env.derive_blocks(self.blocks, expected_safe_head).await;
    }
}

fn security_operator_role() -> B256 {
    keccak256("SECURITY_OPERATOR_ROLE")
}

fn burn_from_role() -> B256 {
    keccak256("BURN_FROM_ROLE")
}
