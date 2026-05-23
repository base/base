//! Stablecoin B-20 precompile action tests across the Base Beryl boundary.

use alloy_consensus::TxReceipt;
use alloy_primitives::{Address, Bytes, TxKind, U256, keccak256};
use alloy_sol_types::{SolCall, SolEvent, SolValue};
use base_common_consensus::{BaseBlock, BaseTxEnvelope};
use base_common_precompiles::{B20FactoryStorage, B20TokenRole, IB20, IB20Factory, IB20Stablecoin};

use crate::env::BerylTestEnv;

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
                StaticcallCase::returndata(
                    "currency",
                    IB20Stablecoin::currencyCall {}.abi_encode(),
                    string_ret(BerylTestEnv::B20_STABLECOIN_CURRENCY),
                ),
                StaticcallCase::returndata(
                    "name",
                    IB20::nameCall {}.abi_encode(),
                    string_ret(BerylTestEnv::B20_STABLECOIN_NAME),
                ),
                StaticcallCase::returndata(
                    "symbol",
                    IB20::symbolCall {}.abi_encode(),
                    string_ret(BerylTestEnv::B20_STABLECOIN_SYMBOL),
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
                StaticcallCase::returndata(
                    "contractURI",
                    IB20::contractURICall {}.abi_encode(),
                    string_ret(""),
                ),
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
async fn stablecoin_calls_revert_while_stablecoin_feature_is_deactivated() {
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
    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "probe transaction must succeed even when the inner staticcall reverts"
    );
    assert!(
        !scenario.env.probe_call_succeeded(probe),
        "currency() staticcall must fail when B20_STABLECOIN is deactivated"
    );

    let transfer_while_deactivated =
        scenario.env.transfer_b20_tx(scenario.token, BerylTestEnv::bob(), U256::ONE);
    let block = scenario.build_block_with_transactions(vec![transfer_while_deactivated]).await;
    assert!(
        !scenario.env.user_tx_succeeded(&block, 0),
        "stablecoin transfer must revert when B20_STABLECOIN is deactivated"
    );
    scenario.assert_total_supply(BerylTestEnv::B20_INITIAL_SUPPLY);
    scenario.assert_balances(BerylTestEnv::B20_INITIAL_SUPPLY, 0, 0);

    let reactivate_stablecoin =
        scenario.env.activate_feature_tx(BerylTestEnv::b20_stablecoin_feature());
    let block = scenario.build_block_with_transactions(vec![reactivate_stablecoin]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "B20_STABLECOIN re-activation must succeed");

    let probe_after_reactivate = scenario.env.call_staticcall_probe_tx(
        probe,
        Bytes::from(IB20Stablecoin::currencyCall {}.abi_encode()),
        BerylTestEnv::B20_PROBE_GAS_LIMIT,
    );
    let block = scenario.build_block_with_transactions(vec![probe_after_reactivate]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "probe transaction must succeed");
    assert!(scenario.env.probe_call_succeeded(probe), "currency() staticcall must succeed again");
    assert_probe_returndata(
        &scenario.env,
        probe,
        "currency after reactivation",
        &string_ret(BerylTestEnv::B20_STABLECOIN_CURRENCY),
    );

    let transfer_after_reactivate =
        scenario.env.transfer_b20_tx(scenario.token, BerylTestEnv::bob(), U256::ONE);
    let block = scenario.build_block_with_transactions(vec![transfer_after_reactivate]).await;
    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "stablecoin transfer must succeed after B20_STABLECOIN is re-activated"
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
    let activate_factory = env.activate_feature_tx(BerylTestEnv::b20_factory_feature());
    let activate_stablecoin = env.activate_feature_tx(BerylTestEnv::b20_stablecoin_feature());
    let block2 = env
        .sequencer
        .build_next_block_with_transactions(vec![activate_factory, activate_stablecoin])
        .await;
    assert!(env.user_tx_succeeded(&block2, 0), "TOKEN_FACTORY activation must succeed");
    assert!(env.user_tx_succeeded(&block2, 1), "B20_STABLECOIN activation must succeed");

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
        let activate_factory = self.env.activate_feature_tx(BerylTestEnv::b20_factory_feature());
        let activate_stablecoin =
            self.env.activate_feature_tx(BerylTestEnv::b20_stablecoin_feature());
        let block =
            self.build_block_with_transactions(vec![activate_factory, activate_stablecoin]).await;

        assert!(self.env.user_tx_succeeded(&block, 0), "TOKEN_FACTORY activation must succeed");
        assert!(self.env.user_tx_succeeded(&block, 1), "B20_STABLECOIN activation must succeed");
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

    async fn assert_staticcall_cases(&mut self, target: Address, cases: Vec<StaticcallCase>) {
        let mut probes = Vec::with_capacity(cases.len());
        let mut deployments = Vec::with_capacity(cases.len());
        for _ in &cases {
            let (probe, deploy) = self.env.deploy_staticcall_probe_tx(target);
            probes.push(probe);
            deployments.push(deploy);
        }

        let deploy_block = self.build_block_with_transactions(deployments).await;
        for index in 0..cases.len() {
            assert!(
                self.env.user_tx_succeeded(&deploy_block, index),
                "stablecoin staticcall probe deployment {index} must succeed"
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
            assert_eq!(
                self.env.probe_return_word(*probe),
                case.expected_word,
                "{} staticcall must return the expected first word",
                case.label
            );
            assert_probe_returndata(&self.env, *probe, case.label, &case.expected_returndata);
        }
    }

    fn assert_total_supply(&self, total_supply: u64) {
        assert_eq!(
            self.env.b20_total_supply(self.token),
            U256::from(total_supply),
            "stablecoin total supply must match expected value"
        );
    }

    fn assert_balances(&self, alice: u64, bob: u64, carol: u64) {
        assert_eq!(
            self.env.b20_balance(self.token, BerylTestEnv::alice()),
            U256::from(alice),
            "Alice stablecoin balance must match expected value"
        );
        assert_eq!(
            self.env.b20_balance(self.token, BerylTestEnv::bob()),
            U256::from(bob),
            "Bob stablecoin balance must match expected value"
        );
        assert_eq!(
            self.env.b20_balance(self.token, BerylTestEnv::carol()),
            U256::from(carol),
            "Carol stablecoin balance must match expected value"
        );
    }

    fn assert_allowance(&self, owner: Address, spender: Address, amount: u64) {
        assert_eq!(
            self.env.b20_allowance(self.token, owner, spender),
            U256::from(amount),
            "stablecoin allowance must match expected value"
        );
    }

    fn assert_created_log(&self, block: &BaseBlock) {
        let expected = IB20Factory::B20Created {
            token: self.token,
            variant: IB20Factory::B20Variant::STABLECOIN,
            name: BerylTestEnv::B20_STABLECOIN_NAME.to_string(),
            symbol: BerylTestEnv::B20_STABLECOIN_SYMBOL.to_string(),
            decimals: BerylTestEnv::B20_STABLECOIN_DECIMALS,
        }
        .encode_log_data();
        assert!(
            self.env
                .user_tx_receipt(block, 0)
                .logs()
                .iter()
                .any(|log| log.address == B20FactoryStorage::ADDRESS && log.data == expected),
            "createB20(STABLECOIN) must emit B20Created"
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

struct StaticcallCase {
    label: &'static str,
    input: Vec<u8>,
    expected_word: U256,
    expected_returndata: Vec<u8>,
}

impl StaticcallCase {
    fn word(label: &'static str, input: Vec<u8>, expected_word: U256) -> Self {
        Self::returndata(label, input, expected_word.abi_encode())
    }

    fn returndata(label: &'static str, input: Vec<u8>, expected_returndata: Vec<u8>) -> Self {
        let expected_word = first_word(&expected_returndata);
        Self { label, input, expected_word, expected_returndata }
    }
}

fn create_stablecoin_with_currency_tx(env: &BerylTestEnv, currency: &str) -> BaseTxEnvelope {
    let params = IB20Factory::B20StablecoinCreateParams {
        version: B20FactoryStorage::CREATE_TOKEN_VERSION,
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

fn assert_probe_returndata(
    env: &BerylTestEnv,
    probe: Address,
    label: &str,
    expected_returndata: &[u8],
) {
    assert_eq!(
        env.probe_return_length(probe),
        U256::from(expected_returndata.len()),
        "{label} staticcall must return the expected byte length"
    );
    assert_eq!(
        env.probe_return_hash(probe),
        returndata_hash_word(expected_returndata),
        "{label} staticcall must return the expected ABI payload"
    );
}

fn string_ret(value: &str) -> Vec<u8> {
    value.to_string().abi_encode()
}

fn first_word(returndata: &[u8]) -> U256 {
    let mut word = [0u8; 32];
    let copied = returndata.len().min(word.len());
    word[..copied].copy_from_slice(&returndata[..copied]);
    U256::from_be_bytes(word)
}

fn returndata_hash_word(returndata: &[u8]) -> U256 {
    U256::from_be_slice(keccak256(returndata).as_slice())
}

fn word_from_address(address: Address) -> U256 {
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(address.as_slice());
    U256::from_be_slice(&word)
}
