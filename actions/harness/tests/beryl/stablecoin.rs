//! Stablecoin B-20 precompile action tests across the Base Beryl boundary.

use alloy_consensus::TxReceipt;
use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_sol_types::{SolCall, SolEvent, SolValue};
use base_common_consensus::{BaseBlock, BaseTxEnvelope};
use base_common_precompiles::{
    B20FactoryStorage, B20TokenRole, B20Variant, IB20, IB20Factory, IB20Stablecoin,
};

use crate::{
    env::BerylTestEnv,
    test_helpers::{self, StaticcallCase, word_from_address},
};

#[tokio::test]
async fn stablecoin_creation_initializes_currency_and_factory_views() {
    let mut scenario = StablecoinScenario::new().await;

    scenario
        .assert_staticcall_cases(
            B20FactoryStorage::ADDRESS,
            vec![
                StaticcallCase::word(
                    "getB20Address(STABLECOIN)",
                    IB20Factory::getB20AddressCall {
                        variant: IB20Factory::B20Variant::STABLECOIN,
                        sender: BerylTestEnv::alice(),
                        salt: BerylTestEnv::b20_stablecoin_salt(),
                    }
                    .abi_encode(),
                    word_from_address(scenario.token),
                ),
                StaticcallCase::word(
                    "isB20(stablecoin)",
                    IB20Factory::isB20Call { token: scenario.token }.abi_encode(),
                    U256::ONE,
                ),
                StaticcallCase::word(
                    "isB20Initialized(stablecoin)",
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
                    "currency",
                    IB20Stablecoin::currencyCall {}.abi_encode(),
                    BerylTestEnv::B20_STABLECOIN_CURRENCY,
                ),
                StaticcallCase::string(
                    "name",
                    IB20::nameCall {}.abi_encode(),
                    BerylTestEnv::B20_STABLECOIN_NAME,
                ),
                StaticcallCase::string(
                    "symbol",
                    IB20::symbolCall {}.abi_encode(),
                    BerylTestEnv::B20_STABLECOIN_SYMBOL,
                ),
                StaticcallCase::word(
                    "decimals",
                    IB20::decimalsCall {}.abi_encode(),
                    U256::from(BerylTestEnv::B20_STABLECOIN_DECIMALS),
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
                    "allowance(alice,bob)",
                    IB20::allowanceCall {
                        owner: BerylTestEnv::alice(),
                        spender: BerylTestEnv::bob(),
                    }
                    .abi_encode(),
                    U256::ZERO,
                ),
                StaticcallCase::word("supplyCap", IB20::supplyCapCall {}.abi_encode(), U256::MAX),
                StaticcallCase::string("contractURI", IB20::contractURICall {}.abi_encode(), ""),
                StaticcallCase::word(
                    "nonces(alice)",
                    IB20::noncesCall { owner: BerylTestEnv::alice() }.abi_encode(),
                    U256::ZERO,
                ),
            ],
        )
        .await;

    scenario.derive().await;
}

#[tokio::test]
async fn stablecoin_inherited_b20_operations_update_state_and_emit_events() {
    let mut scenario = StablecoinScenario::new().await;

    let grant_mint_role = scenario.call_tx(IB20::grantRoleCall {
        role: B20TokenRole::Mint.id(),
        account: BerylTestEnv::alice(),
    });
    let grant_burn_role = scenario.call_tx(IB20::grantRoleCall {
        role: B20TokenRole::Burn.id(),
        account: BerylTestEnv::alice(),
    });
    let block =
        scenario.build_block_with_transactions(vec![grant_mint_role, grant_burn_role]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "MINT_ROLE grant must succeed");
    assert!(scenario.env.user_tx_succeeded(&block, 1), "BURN_ROLE grant must succeed");

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
    let mint_to_carol =
        scenario.call_tx(IB20::mintCall { to: BerylTestEnv::carol(), amount: U256::from(30) });
    let burn_from_alice = scenario.call_tx(IB20::burnCall { amount: U256::from(5) });
    let block = scenario
        .build_block_with_transactions(vec![
            transfer_to_bob,
            approve_bob,
            transfer_from_alice_to_carol,
            mint_to_carol,
            burn_from_alice,
        ])
        .await;

    for index in 0..5 {
        assert!(
            scenario.env.user_tx_succeeded(&block, index),
            "stablecoin inherited B-20 mutation {index} must succeed"
        );
    }
    scenario.assert_transfer_log(
        &block,
        0,
        BerylTestEnv::alice(),
        BerylTestEnv::bob(),
        BerylTestEnv::B20_BOB_TRANSFER,
    );
    scenario.assert_approval_log(
        &block,
        1,
        BerylTestEnv::alice(),
        BerylTestEnv::bob(),
        BerylTestEnv::B20_BOB_ALLOWANCE,
    );
    scenario.assert_transfer_log(
        &block,
        2,
        BerylTestEnv::alice(),
        BerylTestEnv::carol(),
        BerylTestEnv::B20_TRANSFER_FROM_CAROL,
    );
    scenario.assert_transfer_log(&block, 3, Address::ZERO, BerylTestEnv::carol(), 30);
    scenario.assert_transfer_log(&block, 4, BerylTestEnv::alice(), Address::ZERO, 5);
    scenario.assert_total_supply(BerylTestEnv::B20_INITIAL_SUPPLY + 25);
    scenario.assert_balances(
        BerylTestEnv::B20_INITIAL_SUPPLY
            - BerylTestEnv::B20_BOB_TRANSFER
            - BerylTestEnv::B20_TRANSFER_FROM_CAROL
            - 5,
        BerylTestEnv::B20_BOB_TRANSFER,
        BerylTestEnv::B20_TRANSFER_FROM_CAROL + 30,
    );
    scenario.assert_allowance(
        BerylTestEnv::alice(),
        BerylTestEnv::bob(),
        BerylTestEnv::B20_BOB_ALLOWANCE - BerylTestEnv::B20_TRANSFER_FROM_CAROL,
    );

    scenario.derive().await;
}

#[tokio::test]
async fn stablecoin_calls_succeed_while_stablecoin_feature_is_deactivated() {
    let mut scenario = StablecoinScenario::new().await;

    let (probe, deploy_probe) = scenario.env.deploy_staticcall_probe_tx(scenario.token);
    let block = scenario.build_block_with_transactions(vec![deploy_probe]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "stablecoin probe must deploy");

    let deactivate_stablecoin =
        scenario.env.deactivate_feature_tx(BerylTestEnv::b20_stablecoin_feature());
    let block = scenario.build_block_with_transactions(vec![deactivate_stablecoin]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "B20_STABLECOIN deactivation must succeed");

    let probe_while_deactivated = scenario.env.call_staticcall_probe_tx(
        probe,
        Bytes::from(IB20Stablecoin::currencyCall {}.abi_encode()),
        BerylTestEnv::B20_PROBE_GAS_LIMIT,
    );
    let block = scenario.build_block_with_transactions(vec![probe_while_deactivated]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "probe transaction must succeed");
    assert!(
        scenario.env.probe_call_succeeded(probe),
        "currency() staticcall must succeed on existing token even when B20_STABLECOIN is deactivated"
    );

    let transfer_while_deactivated =
        scenario.env.transfer_b20_tx(scenario.token, BerylTestEnv::bob(), U256::ONE);
    let block = scenario.build_block_with_transactions(vec![transfer_while_deactivated]).await;
    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "existing stablecoin transfer must succeed even when B20_STABLECOIN is deactivated"
    );
    scenario.assert_transfer_log(&block, 0, BerylTestEnv::alice(), BerylTestEnv::bob(), 1);
    scenario.assert_balances(BerylTestEnv::B20_INITIAL_SUPPLY - 1, 1, 0);

    scenario.derive().await;
}

#[tokio::test]
async fn stablecoin_creation_reverts_for_invalid_currency() {
    let mut env = BerylTestEnv::new();
    let token = env.b20_stablecoin_address();

    let block1 = env.sequencer.build_empty_block().await;
    let activate_stablecoin = env.activate_feature_tx(BerylTestEnv::b20_stablecoin_feature());
    let block2 = env.sequencer.build_next_block_with_transactions(vec![activate_stablecoin]).await;
    assert!(env.user_tx_succeeded(&block2, 0), "B20_STABLECOIN activation must succeed");

    let invalid_currency = create_stablecoin_with_currency_tx(&env, "usd");
    let block3 = env.sequencer.build_next_block_with_transactions(vec![invalid_currency]).await;

    assert!(!env.user_tx_succeeded(&block3, 0), "lowercase stablecoin currency must revert");
    assert!(!env.sequencer.has_code(token), "invalid stablecoin creation must not deploy code");
    assert_eq!(
        env.b20_total_supply(token),
        U256::ZERO,
        "invalid stablecoin creation must not initialize supply"
    );

    env.derive_blocks([(block1, 1), (block2, 2), (block3, 3)], 3).await;
}

struct StablecoinScenario {
    env: BerylTestEnv,
    token: Address,
    blocks: Vec<(BaseBlock, u64)>,
}

impl StablecoinScenario {
    async fn new() -> Self {
        let env = BerylTestEnv::new();
        let token = env.b20_stablecoin_address();
        let mut scenario = Self { env, token, blocks: Vec::new() };

        scenario.build_block_with_transactions(Vec::new()).await;
        scenario.activate_precompiles().await;

        let create = scenario.env.create_b20_stablecoin_tx();
        let block = scenario.build_block_with_transactions(vec![create]).await;

        assert!(
            scenario.env.user_tx_succeeded(&block, 0),
            "stablecoin creation transaction must succeed"
        );
        assert!(scenario.env.sequencer.has_code(token), "stablecoin token code must be deployed");
        scenario.assert_created_log(&block);
        scenario.assert_transfer_log(
            &block,
            0,
            Address::ZERO,
            BerylTestEnv::alice(),
            BerylTestEnv::B20_INITIAL_SUPPLY,
        );
        scenario.assert_total_supply(BerylTestEnv::B20_INITIAL_SUPPLY);
        scenario.assert_balances(BerylTestEnv::B20_INITIAL_SUPPLY, 0, 0);

        scenario
    }

    async fn activate_precompiles(&mut self) {
        let activate_stablecoin =
            self.env.activate_feature_tx(BerylTestEnv::b20_stablecoin_feature());
        let block = self.build_block_with_transactions(vec![activate_stablecoin]).await;

        assert!(self.env.user_tx_succeeded(&block, 0), "B20_STABLECOIN activation must succeed");
    }

    async fn build_block_with_transactions(
        &mut self,
        transactions: Vec<BaseTxEnvelope>,
    ) -> BaseBlock {
        test_helpers::build_block_with_transactions(&mut self.env, &mut self.blocks, transactions)
            .await
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
            "stablecoin",
        )
        .await;
    }

    fn assert_total_supply(&self, total_supply: u64) {
        test_helpers::assert_total_supply(&self.env, self.token, "stablecoin", total_supply);
    }

    fn assert_balances(&self, alice: u64, bob: u64, carol: u64) {
        test_helpers::assert_balances(&self.env, self.token, "stablecoin", alice, bob, carol);
    }

    fn assert_allowance(&self, owner: Address, spender: Address, amount: u64) {
        assert_eq!(
            self.env.b20_allowance(self.token, owner, spender),
            U256::from(amount),
            "stablecoin allowance must match expected value"
        );
    }

    fn assert_created_log(&self, block: &BaseBlock) {
        let variant_params: Bytes = IB20Factory::B20StablecoinEventParams {
            version: 1,
            currency: BerylTestEnv::B20_STABLECOIN_CURRENCY.to_string(),
        }
        .abi_encode()
        .into();
        let expected = IB20Factory::B20Created {
            token: self.token,
            variant: IB20Factory::B20Variant::STABLECOIN,
            name: BerylTestEnv::B20_STABLECOIN_NAME.to_string(),
            symbol: BerylTestEnv::B20_STABLECOIN_SYMBOL.to_string(),
            decimals: BerylTestEnv::B20_STABLECOIN_DECIMALS,
            variantParams: variant_params,
        }
        .encode_log_data();
        assert!(
            self.env
                .user_tx_receipt(block, 0)
                .logs()
                .iter()
                .any(|log| log.address == B20FactoryStorage::ADDRESS && log.data == expected),
            "createB20(STABLECOIN) must emit B20Created with ABI-encoded currency in variantParams"
        );
    }

    fn assert_transfer_log(
        &self,
        block: &BaseBlock,
        user_tx_index: usize,
        from: Address,
        to: Address,
        amount: u64,
    ) {
        assert!(
            self.env.b20_transfer_log_emitted(
                block,
                user_tx_index,
                self.token,
                from,
                to,
                U256::from(amount),
            ),
            "stablecoin transaction {user_tx_index} must emit a Transfer event"
        );
    }

    fn assert_approval_log(
        &self,
        block: &BaseBlock,
        user_tx_index: usize,
        owner: Address,
        spender: Address,
        amount: u64,
    ) {
        assert!(
            self.env.b20_approval_log_emitted(
                block,
                user_tx_index,
                self.token,
                owner,
                spender,
                U256::from(amount),
            ),
            "stablecoin transaction {user_tx_index} must emit an Approval event"
        );
    }

    async fn derive(mut self) {
        let expected_safe_head = self.blocks.len() as u64;
        self.env.derive_blocks(self.blocks, expected_safe_head).await;
    }
}

fn create_stablecoin_with_currency_tx(env: &BerylTestEnv, currency: &str) -> BaseTxEnvelope {
    let params = IB20Factory::B20StablecoinCreateParams {
        version: B20Variant::Stablecoin.supported_version(),
        name: BerylTestEnv::B20_STABLECOIN_NAME.to_string(),
        symbol: BerylTestEnv::B20_STABLECOIN_SYMBOL.to_string(),
        initialAdmin: BerylTestEnv::alice(),
        currency: currency.to_string(),
    };

    env.create_tx(
        TxKind::Call(B20FactoryStorage::ADDRESS),
        Bytes::from(
            IB20Factory::createB20Call {
                variant: IB20Factory::B20Variant::STABLECOIN,
                salt: BerylTestEnv::b20_stablecoin_salt(),
                params: params.abi_encode().into(),
                initCalls: Vec::new(),
            }
            .abi_encode(),
        ),
        BerylTestEnv::B20_GAS_LIMIT,
    )
}
