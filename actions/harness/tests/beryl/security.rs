//! Asset B-20 precompile action tests across the Base Beryl boundary.

use alloy_consensus::TxReceipt;
use alloy_primitives::{Address, B256, Bytes, LogData, TxKind, U256, keccak256};
use alloy_sol_types::{SolCall, SolEvent, SolValue};
use base_common_consensus::{BaseBlock, BaseTxEnvelope};
use base_common_precompiles::{B20FactoryStorage, B20TokenRole, IB20, IB20Asset, IB20Factory};

use crate::{
    env::BerylTestEnv,
    test_helpers::{self, StaticcallCase, word_from_address},
};

const WAD: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);
const UPDATED_MULTIPLIER: U256 = U256::from_limbs([2_000_000_000_000_000_000, 0, 0, 0]);
const BOB_MINT_AMOUNT: u64 = 100;
const CAROL_MINT_AMOUNT: u64 = 200;
const CUSIP: &str = "123456789";
const FIGI: &str = "BBG000000001";
const ANNOUNCEMENT_ID: &str = "security-action-1";
const ANNOUNCEMENT_DESCRIPTION: &str = "update FIGI";
const ANNOUNCEMENT_URI: &str = "ipfs://security-action";

#[tokio::test]
async fn security_creation_initializes_identifiers_and_factory_views() {
    let mut scenario = B20AssetScenario::new().await;

    scenario
        .assert_staticcall_cases(
            B20FactoryStorage::ADDRESS,
            vec![
                StaticcallCase::word(
                    "factory getB20Address(SECURITY)",
                    IB20Factory::getB20AddressCall {
                        variant: IB20Factory::B20Variant::ASSET,
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
                    BerylTestEnv::B20_ASSET_NAME,
                ),
                StaticcallCase::string(
                    "symbol",
                    IB20::symbolCall {}.abi_encode(),
                    BerylTestEnv::B20_ASSET_SYMBOL,
                ),
                StaticcallCase::string("contractURI", IB20::contractURICall {}.abi_encode(), ""),
                StaticcallCase::string(
                    "extraMetadata(unset)",
                    IB20Asset::extraMetadataCall { key: "ISIN".to_string() }.abi_encode(),
                    "",
                ),
                StaticcallCase::word(
                    "decimals",
                    IB20::decimalsCall {}.abi_encode(),
                    U256::from(BerylTestEnv::B20_ASSET_DECIMALS),
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
                StaticcallCase::word("multiplier", IB20Asset::multiplierCall {}.abi_encode(), WAD),
                StaticcallCase::word(
                    "WAD_PRECISION",
                    IB20Asset::WAD_PRECISIONCall {}.abi_encode(),
                    WAD,
                ),
                StaticcallCase::word(
                    "toScaledBalance",
                    IB20Asset::toScaledBalanceCall { rawBalance: U256::from(100) }.abi_encode(),
                    U256::from(100),
                ),
                StaticcallCase::word(
                    "toRawBalance",
                    IB20Asset::toRawBalanceCall { scaledBalance: U256::from(100) }.abi_encode(),
                    U256::from(100),
                ),
                StaticcallCase::word(
                    "scaledBalanceOf((alice)",
                    IB20Asset::scaledBalanceOfCall { account: BerylTestEnv::alice() }.abi_encode(),
                    U256::from(BerylTestEnv::B20_INITIAL_SUPPLY),
                ),
                StaticcallCase::word(
                    "isAnnouncementIdUsed(fresh)",
                    IB20Asset::isAnnouncementIdUsedCall { id: ANNOUNCEMENT_ID.to_string() }
                        .abi_encode(),
                    U256::ZERO,
                ),
                StaticcallCase::bytes32(
                    "OPERATOR_ROLE",
                    IB20Asset::OPERATOR_ROLECall {}.abi_encode(),
                    operator_role(),
                ),
                StaticcallCase::bytes32(
                    "METADATA_ROLE",
                    IB20::METADATA_ROLECall {}.abi_encode(),
                    metadata_role(),
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
    let mut scenario = B20AssetScenario::new().await;
    scenario.grant_roles([operator_role(), metadata_role(), B20TokenRole::Mint.id()]).await;

    let update_multiplier =
        scenario.call_tx(IB20Asset::updateMultiplierCall { newMultiplier: UPDATED_MULTIPLIER });
    let update_cusip = scenario.call_tx(IB20Asset::updateExtraMetadataCall {
        key: "CUSIP".to_string(),
        value: CUSIP.to_string(),
    });
    let batch_mint = scenario.call_tx(IB20Asset::batchMintCall {
        recipients: vec![BerylTestEnv::bob(), BerylTestEnv::carol()],
        amounts: vec![U256::from(BOB_MINT_AMOUNT), U256::from(CAROL_MINT_AMOUNT)],
    });
    let announced_identifier =
        IB20Asset::updateExtraMetadataCall { key: "FIGI".to_string(), value: FIGI.to_string() };
    let announce = scenario.call_tx(IB20Asset::announceCall {
        internalCalls: vec![Bytes::from(announced_identifier.abi_encode())],
        id: ANNOUNCEMENT_ID.to_string(),
        description: ANNOUNCEMENT_DESCRIPTION.to_string(),
        uri: ANNOUNCEMENT_URI.to_string(),
    });
    let block = scenario
        .build_block_with_transactions(vec![update_multiplier, update_cusip, batch_mint, announce])
        .await;

    for index in 0..4 {
        assert!(
            scenario.env.user_tx_succeeded(&block, index),
            "security mutation {index} must succeed"
        );
    }

    scenario.assert_log(
        &block,
        0,
        IB20Asset::MultiplierUpdated { multiplier: UPDATED_MULTIPLIER }.encode_log_data(),
    );
    scenario.assert_log(
        &block,
        1,
        IB20Asset::ExtraMetadataUpdated { key: "CUSIP".to_string(), value: CUSIP.to_string() }
            .encode_log_data(),
    );
    scenario.assert_log(
        &block,
        2,
        IB20::Transfer {
            from: Address::ZERO,
            to: BerylTestEnv::bob(),
            amount: U256::from(BOB_MINT_AMOUNT),
        }
        .encode_log_data(),
    );
    scenario.assert_log(
        &block,
        2,
        IB20::Transfer {
            from: Address::ZERO,
            to: BerylTestEnv::carol(),
            amount: U256::from(CAROL_MINT_AMOUNT),
        }
        .encode_log_data(),
    );
    scenario.assert_log(
        &block,
        3,
        IB20Asset::Announcement {
            caller: BerylTestEnv::alice(),
            id: ANNOUNCEMENT_ID.to_string(),
            description: ANNOUNCEMENT_DESCRIPTION.to_string(),
            uri: ANNOUNCEMENT_URI.to_string(),
        }
        .encode_log_data(),
    );
    scenario.assert_log(
        &block,
        3,
        IB20Asset::ExtraMetadataUpdated { key: "FIGI".to_string(), value: FIGI.to_string() }
            .encode_log_data(),
    );
    scenario.assert_log(
        &block,
        3,
        IB20Asset::EndAnnouncement { id: ANNOUNCEMENT_ID.to_string() }.encode_log_data(),
    );

    scenario.assert_total_supply(
        BerylTestEnv::B20_INITIAL_SUPPLY + BOB_MINT_AMOUNT + CAROL_MINT_AMOUNT,
    );
    scenario.assert_balances(BerylTestEnv::B20_INITIAL_SUPPLY, BOB_MINT_AMOUNT, CAROL_MINT_AMOUNT);

    scenario
        .assert_staticcall_cases(
            scenario.token,
            vec![
                StaticcallCase::word(
                    "multiplier after update",
                    IB20Asset::multiplierCall {}.abi_encode(),
                    UPDATED_MULTIPLIER,
                ),
                StaticcallCase::word(
                    "toScaledBalance after update",
                    IB20Asset::toScaledBalanceCall { rawBalance: U256::from(50) }.abi_encode(),
                    U256::from(100),
                ),
                StaticcallCase::word(
                    "toRawBalance after update",
                    IB20Asset::toRawBalanceCall { scaledBalance: U256::from(100) }.abi_encode(),
                    U256::from(50),
                ),
                StaticcallCase::word(
                    "scaledBalanceOf((alice) after update",
                    IB20Asset::scaledBalanceOfCall { account: BerylTestEnv::alice() }.abi_encode(),
                    U256::from(BerylTestEnv::B20_INITIAL_SUPPLY) * U256::from(2),
                ),
                StaticcallCase::string(
                    "extraMetadata(CUSIP)",
                    IB20Asset::extraMetadataCall { key: "CUSIP".to_string() }.abi_encode(),
                    CUSIP,
                ),
                StaticcallCase::string(
                    "extraMetadata(FIGI)",
                    IB20Asset::extraMetadataCall { key: "FIGI".to_string() }.abi_encode(),
                    FIGI,
                ),
                StaticcallCase::word(
                    "isAnnouncementIdUsed",
                    IB20Asset::isAnnouncementIdUsedCall { id: ANNOUNCEMENT_ID.to_string() }
                        .abi_encode(),
                    U256::ONE,
                ),
                StaticcallCase::word(
                    "totalSupply after security mutations",
                    IB20::totalSupplyCall {}.abi_encode(),
                    U256::from(1_000_300),
                ),
            ],
        )
        .await;

    scenario.derive().await;
}

#[tokio::test]
async fn security_mutations_revert_on_invalid_inputs() {
    let mut scenario = B20AssetScenario::new().await;
    scenario.grant_roles([operator_role(), metadata_role(), B20TokenRole::Mint.id()]).await;

    let first_announcement = scenario.call_tx(IB20Asset::announceCall {
        internalCalls: Vec::new(),
        id: "duplicate-id".to_string(),
        description: "initial".to_string(),
        uri: "ipfs://initial".to_string(),
    });
    let empty_batch_mint =
        scenario.call_tx(IB20Asset::batchMintCall { recipients: Vec::new(), amounts: Vec::new() });
    let mismatched_batch_mint = scenario.call_tx(IB20Asset::batchMintCall {
        recipients: vec![BerylTestEnv::bob()],
        amounts: vec![U256::from(1), U256::from(2)],
    });
    let empty_metadata_key = scenario
        .call_tx(IB20Asset::updateExtraMetadataCall { key: String::new(), value: "x".to_string() });
    let duplicate_announcement = scenario.call_tx(IB20Asset::announceCall {
        internalCalls: Vec::new(),
        id: "duplicate-id".to_string(),
        description: "again".to_string(),
        uri: "ipfs://again".to_string(),
    });
    let malformed_internal_call = scenario.call_tx(IB20Asset::announceCall {
        internalCalls: vec![Bytes::from(vec![1, 2, 3])],
        id: "malformed-id".to_string(),
        description: "malformed".to_string(),
        uri: "ipfs://malformed".to_string(),
    });
    let recursive_call = IB20Asset::announceCall {
        internalCalls: Vec::new(),
        id: "inner".to_string(),
        description: "inner".to_string(),
        uri: "ipfs://inner".to_string(),
    };
    let recursive_announcement = scenario.call_tx(IB20Asset::announceCall {
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
            empty_metadata_key,
            duplicate_announcement,
            malformed_internal_call,
            recursive_announcement,
        ])
        .await;

    assert!(scenario.env.user_tx_succeeded(&block, 0), "first announce() must succeed");
    for index in 1..7 {
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
                    IB20Asset::isAnnouncementIdUsedCall { id: "duplicate-id".to_string() }
                        .abi_encode(),
                    U256::ONE,
                ),
                StaticcallCase::word(
                    "failed malformed announcement id is rolled back",
                    IB20Asset::isAnnouncementIdUsedCall { id: "malformed-id".to_string() }
                        .abi_encode(),
                    U256::ZERO,
                ),
                StaticcallCase::word(
                    "failed recursive announcement id is rolled back",
                    IB20Asset::isAnnouncementIdUsedCall { id: "recursive-id".to_string() }
                        .abi_encode(),
                    U256::ZERO,
                ),
            ],
        )
        .await;

    scenario.derive().await;
}

#[tokio::test]
async fn security_calls_succeed_while_security_feature_is_deactivated() {
    let mut scenario = B20AssetScenario::new().await;

    let deactivate_security = scenario.env.deactivate_feature_tx(BerylTestEnv::b20_asset_feature());
    let block = scenario.build_block_with_transactions(vec![deactivate_security]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "B20_SECURITY deactivation must succeed");

    let transfer_while_deactivated =
        scenario.call_tx(IB20::transferCall { to: BerylTestEnv::bob(), amount: U256::from(1) });
    let block = scenario.build_block_with_transactions(vec![transfer_while_deactivated]).await;
    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "existing security token call must succeed even when B20_SECURITY is deactivated"
    );
    scenario.assert_balances(BerylTestEnv::B20_INITIAL_SUPPLY - 1, 1, 0);

    let (probe, deploy_probe) = scenario.env.deploy_staticcall_probe_tx(scenario.token);
    let block = scenario.build_block_with_transactions(vec![deploy_probe]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "staticcall probe must deploy");

    let probe_call = scenario.env.call_staticcall_probe_tx(
        probe,
        Bytes::from(IB20Asset::multiplierCall {}.abi_encode()),
        BerylTestEnv::B20_PROBE_GAS_LIMIT,
    );
    let block = scenario.build_block_with_transactions(vec![probe_call]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "probe transaction must succeed");
    assert!(
        scenario.env.probe_call_succeeded(probe),
        "existing security staticcall must succeed even when B20_SECURITY is deactivated"
    );

    scenario.derive().await;
}

struct B20AssetScenario {
    env: BerylTestEnv,
    token: Address,
    blocks: Vec<(BaseBlock, u64)>,
}

impl B20AssetScenario {
    async fn new() -> Self {
        let env = BerylTestEnv::new();
        let token = env.b20_security_address();
        let mut scenario = Self { env, token, blocks: Vec::new() };

        scenario.build_block_with_transactions(Vec::new()).await;

        let activate_security = scenario.env.activate_feature_tx(BerylTestEnv::b20_asset_feature());
        let activate_policy =
            scenario.env.activate_feature_tx(BerylTestEnv::policy_registry_feature());
        let block =
            scenario.build_block_with_transactions(vec![activate_security, activate_policy]).await;

        assert!(scenario.env.user_tx_succeeded(&block, 0), "B20_SECURITY activation must succeed");
        assert!(
            scenario.env.user_tx_succeeded(&block, 1),
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
            variant: IB20Factory::B20Variant::ASSET,
            name: BerylTestEnv::B20_ASSET_NAME.to_string(),
            symbol: BerylTestEnv::B20_ASSET_SYMBOL.to_string(),
            decimals: BerylTestEnv::B20_ASSET_DECIMALS,
            variantParams: Bytes::new(),
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

fn operator_role() -> B256 {
    keccak256("OPERATOR_ROLE")
}

fn metadata_role() -> B256 {
    keccak256("METADATA_ROLE")
}
