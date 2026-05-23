//! Stablecoin B-20 precompile action tests across the Base Beryl boundary.

use alloy_consensus::TxReceipt;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256};
use alloy_sol_types::{SolCall, SolEvent, SolValue};
use base_common_consensus::{BaseBlock, BaseTxEnvelope};
use base_common_precompiles::{
    ActivationFeature, B20FactoryStorage, B20TokenRole, B20Variant, IB20, IB20Factory,
    IB20Stablecoin,
};

use crate::env::BerylTestEnv;

/// Token name for the test stablecoin.
const STABLECOIN_NAME: &str = "Action Stablecoin";

/// Token symbol for the test stablecoin.
const STABLECOIN_SYMBOL: &str = "AUSD";

/// ISO 4217 currency code for the test stablecoin.
const STABLECOIN_CURRENCY: &str = "USD";

/// Salt used when computing the deterministic stablecoin token address.
const STABLECOIN_SALT: B256 = B256::repeat_byte(0x55);

/// Fixed decimal precision for the stablecoin B-20 variant.
const STABLECOIN_TOKEN_DECIMALS: u8 = 6;

/// Memo value used in the transferWithMemo test.
const STABLECOIN_MEMO: B256 = B256::ZERO;

/// Verifies that creating a stablecoin deploys code at the token address and emits a B20Created event.
#[tokio::test]
async fn stablecoin_creation_deploys_code_and_emits_created_event() {
    let scenario = B20StablecoinScenario::new().await;

    assert!(
        scenario.env.sequencer.has_code(scenario.token),
        "stablecoin token code must be deployed at the deterministic address after creation"
    );

    let expected = IB20Factory::B20Created {
        token: scenario.token,
        variant: IB20Factory::B20Variant::STABLECOIN,
        name: STABLECOIN_NAME.to_string(),
        symbol: STABLECOIN_SYMBOL.to_string(),
        decimals: STABLECOIN_TOKEN_DECIMALS,
    }
    .encode_log_data();

    assert!(
        scenario
            .env
            .user_tx_receipt(&scenario.creation_block, 0)
            .logs()
            .iter()
            .any(|log| log.address == B20FactoryStorage::ADDRESS && log.data == expected),
        "stablecoin creation must emit a B20Created event on the factory address"
    );

    scenario.derive().await;
}

/// Verifies that the stablecoin currency() ABI call returns an ABI-encoded string.
#[tokio::test]
async fn stablecoin_currency_abi_call_returns_stored_value() {
    let mut scenario = B20StablecoinScenario::new().await;

    scenario
        .assert_staticcall_cases(vec![StaticcallCase::word(
            "currency",
            IB20Stablecoin::currencyCall {}.abi_encode(),
            U256::from(32),
        )])
        .await;

    scenario.derive().await;
}

/// Verifies that a stablecoin transfer updates balances and emits a Transfer event.
#[tokio::test]
async fn stablecoin_transfer_updates_balances_and_emits_transfer_event() {
    let mut scenario = B20StablecoinScenario::new().await;

    let transfer = scenario.call_tx(IB20::transferCall {
        to: BerylTestEnv::bob(),
        amount: U256::from(BerylTestEnv::B20_BOB_TRANSFER),
    });
    let block = scenario.build_block(vec![transfer]).await;

    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "stablecoin transfer from alice to bob must succeed"
    );
    assert!(
        scenario.env.b20_transfer_log_emitted(
            &block,
            0,
            scenario.token,
            BerylTestEnv::alice(),
            BerylTestEnv::bob(),
            U256::from(BerylTestEnv::B20_BOB_TRANSFER),
        ),
        "stablecoin transfer must emit a Transfer event"
    );
    scenario.assert_balance(
        BerylTestEnv::alice(),
        BerylTestEnv::B20_INITIAL_SUPPLY - BerylTestEnv::B20_BOB_TRANSFER,
    );
    scenario.assert_balance(BerylTestEnv::bob(), BerylTestEnv::B20_BOB_TRANSFER);
    scenario.assert_total_supply(BerylTestEnv::B20_INITIAL_SUPPLY);

    scenario.derive().await;
}

/// Verifies that the factory getB20Address view returns the correct stablecoin address.
#[tokio::test]
async fn stablecoin_get_address_returns_correct_address_for_stablecoin_variant() {
    let mut scenario = B20StablecoinScenario::new().await;

    let (factory_probe, deploy_factory_probe) =
        scenario.env.deploy_staticcall_probe_tx(B20FactoryStorage::ADDRESS);
    let block = scenario.build_block(vec![deploy_factory_probe]).await;
    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "factory staticcall probe must deploy successfully"
    );

    let query = scenario.env.call_staticcall_probe_tx(
        factory_probe,
        Bytes::from(
            IB20Factory::getB20AddressCall {
                variant: IB20Factory::B20Variant::STABLECOIN,
                sender: BerylTestEnv::alice(),
                salt: STABLECOIN_SALT,
            }
            .abi_encode(),
        ),
        BerylTestEnv::B20_PROBE_GAS_LIMIT,
    );
    let _block = scenario.build_block(vec![query]).await;

    assert!(
        scenario.env.probe_call_succeeded(factory_probe),
        "getB20Address staticcall must succeed for the stablecoin variant"
    );
    assert_eq!(
        scenario.env.probe_return_word(factory_probe),
        word_from_address(scenario.token),
        "getB20Address must return the deterministic stablecoin token address"
    );

    scenario.derive().await;
}

/// Verifies that minting increases total supply and the recipient balance.
#[tokio::test]
async fn stablecoin_mint_increases_supply_and_balance() {
    let mut scenario = B20StablecoinScenario::new().await;

    let grant_mint = scenario.call_tx(IB20::grantRoleCall {
        role: B20TokenRole::Mint.id(),
        account: BerylTestEnv::alice(),
    });
    let block = scenario.build_block(vec![grant_mint]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "MINT_ROLE grant to alice must succeed");

    const MINT_AMOUNT: u64 = 500;
    let mint = scenario
        .call_tx(IB20::mintCall { to: BerylTestEnv::alice(), amount: U256::from(MINT_AMOUNT) });
    let block = scenario.build_block(vec![mint]).await;

    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "mint must succeed after MINT_ROLE is granted"
    );
    scenario.assert_total_supply(BerylTestEnv::B20_INITIAL_SUPPLY + MINT_AMOUNT);
    scenario.assert_balance(BerylTestEnv::alice(), BerylTestEnv::B20_INITIAL_SUPPLY + MINT_AMOUNT);

    scenario.derive().await;
}

/// Verifies that burning decreases total supply and the burner balance.
#[tokio::test]
async fn stablecoin_burn_decreases_supply_and_balance() {
    let mut scenario = B20StablecoinScenario::new().await;

    let grant_burn = scenario.call_tx(IB20::grantRoleCall {
        role: B20TokenRole::Burn.id(),
        account: BerylTestEnv::alice(),
    });
    let block = scenario.build_block(vec![grant_burn]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "BURN_ROLE grant to alice must succeed");

    const BURN_AMOUNT: u64 = 1_000;
    let burn = scenario.call_tx(IB20::burnCall { amount: U256::from(BURN_AMOUNT) });
    let block = scenario.build_block(vec![burn]).await;

    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "burn must succeed after BURN_ROLE is granted"
    );
    scenario.assert_total_supply(BerylTestEnv::B20_INITIAL_SUPPLY - BURN_AMOUNT);
    scenario.assert_balance(BerylTestEnv::alice(), BerylTestEnv::B20_INITIAL_SUPPLY - BURN_AMOUNT);

    scenario.derive().await;
}

/// Verifies that approve sets the allowance and emits an Approval event.
#[tokio::test]
async fn stablecoin_approve_updates_allowance_and_emits_approval_event() {
    let mut scenario = B20StablecoinScenario::new().await;

    const ALLOWANCE: u64 = 5_000;
    let approve = scenario
        .call_tx(IB20::approveCall { spender: BerylTestEnv::bob(), amount: U256::from(ALLOWANCE) });
    let block = scenario.build_block(vec![approve]).await;

    assert!(scenario.env.user_tx_succeeded(&block, 0), "stablecoin approve must succeed");
    assert!(
        scenario.env.b20_approval_log_emitted(
            &block,
            0,
            scenario.token,
            BerylTestEnv::alice(),
            BerylTestEnv::bob(),
            U256::from(ALLOWANCE),
        ),
        "approve must emit an Approval event"
    );
    assert_eq!(
        scenario.env.b20_allowance(scenario.token, BerylTestEnv::alice(), BerylTestEnv::bob()),
        U256::from(ALLOWANCE),
        "allowance must equal the approved amount after approve"
    );

    scenario.derive().await;
}

/// Verifies that transferFrom consumes allowance and emits a Transfer event.
#[tokio::test]
async fn stablecoin_transfer_from_uses_allowance_and_emits_transfer() {
    let mut scenario = B20StablecoinScenario::new().await;

    const ALLOWANCE: u64 = 5_000;
    const TRANSFER: u64 = 2_000;

    let approve = scenario
        .call_tx(IB20::approveCall { spender: BerylTestEnv::bob(), amount: U256::from(ALLOWANCE) });
    let block = scenario.build_block(vec![approve]).await;
    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "stablecoin approve must succeed before transferFrom"
    );

    let transfer_from = scenario.bob_call_tx(IB20::transferFromCall {
        from: BerylTestEnv::alice(),
        to: BerylTestEnv::carol(),
        amount: U256::from(TRANSFER),
    });
    let block = scenario.build_block(vec![transfer_from]).await;

    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "transferFrom must succeed within the approved allowance"
    );
    assert!(
        scenario.env.b20_transfer_log_emitted(
            &block,
            0,
            scenario.token,
            BerylTestEnv::alice(),
            BerylTestEnv::carol(),
            U256::from(TRANSFER),
        ),
        "transferFrom must emit a Transfer event"
    );
    assert_eq!(
        scenario.env.b20_allowance(scenario.token, BerylTestEnv::alice(), BerylTestEnv::bob()),
        U256::from(ALLOWANCE - TRANSFER),
        "allowance must decrease by the transferred amount after transferFrom"
    );

    scenario.derive().await;
}

/// Verifies that a zero-amount transfer succeeds without changing balances.
#[tokio::test]
async fn stablecoin_transfer_zero_amount_succeeds() {
    let mut scenario = B20StablecoinScenario::new().await;

    let zero_transfer =
        scenario.call_tx(IB20::transferCall { to: BerylTestEnv::bob(), amount: U256::ZERO });
    let block = scenario.build_block(vec![zero_transfer]).await;

    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "zero-amount stablecoin transfer must succeed"
    );
    scenario.assert_balance(BerylTestEnv::alice(), BerylTestEnv::B20_INITIAL_SUPPLY);
    scenario.assert_balance(BerylTestEnv::bob(), 0);
    scenario.assert_total_supply(BerylTestEnv::B20_INITIAL_SUPPLY);

    scenario.derive().await;
}

/// Verifies that a transfer reverts when the sender has insufficient balance.
#[tokio::test]
async fn stablecoin_transfer_reverts_on_insufficient_balance() {
    let mut scenario = B20StablecoinScenario::new().await;

    let overdraw = scenario.call_tx(IB20::transferCall {
        to: BerylTestEnv::bob(),
        amount: U256::from(BerylTestEnv::B20_INITIAL_SUPPLY) + U256::ONE,
    });
    let block = scenario.build_block(vec![overdraw]).await;

    assert!(
        !scenario.env.user_tx_succeeded(&block, 0),
        "stablecoin transfer must revert when sender balance is insufficient"
    );
    scenario.assert_balance(BerylTestEnv::alice(), BerylTestEnv::B20_INITIAL_SUPPLY);
    scenario.assert_balance(BerylTestEnv::bob(), 0);

    scenario.derive().await;
}

/// Verifies that updateSupplyCap enforces a mint ceiling at the new cap.
#[tokio::test]
async fn stablecoin_update_supply_cap_enforces_mint_limit() {
    let mut scenario = B20StablecoinScenario::new().await;

    let grant_mint = scenario.call_tx(IB20::grantRoleCall {
        role: B20TokenRole::Mint.id(),
        account: BerylTestEnv::alice(),
    });
    let block = scenario.build_block(vec![grant_mint]).await;
    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "MINT_ROLE grant must succeed before testing supply cap"
    );

    let current_supply = U256::from(BerylTestEnv::B20_INITIAL_SUPPLY);
    let update_cap = scenario.call_tx(IB20::updateSupplyCapCall { newSupplyCap: current_supply });
    let block = scenario.build_block(vec![update_cap]).await;
    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "updateSupplyCap must succeed when called by the DefaultAdmin"
    );

    let mint_over_cap =
        scenario.call_tx(IB20::mintCall { to: BerylTestEnv::alice(), amount: U256::ONE });
    let block = scenario.build_block(vec![mint_over_cap]).await;

    assert!(
        !scenario.env.user_tx_succeeded(&block, 0),
        "mint must revert when the supply cap has been reached"
    );
    scenario.assert_total_supply(BerylTestEnv::B20_INITIAL_SUPPLY);

    scenario.derive().await;
}

/// Verifies that the name() view returns an ABI-encoded string.
#[tokio::test]
async fn stablecoin_name_returns_token_name() {
    let mut scenario = B20StablecoinScenario::new().await;

    scenario
        .assert_staticcall_cases(vec![StaticcallCase::word(
            "name",
            IB20::nameCall {}.abi_encode(),
            U256::from(32),
        )])
        .await;

    scenario.derive().await;
}

/// Verifies that the symbol() view returns an ABI-encoded string.
#[tokio::test]
async fn stablecoin_symbol_returns_token_symbol() {
    let mut scenario = B20StablecoinScenario::new().await;

    scenario
        .assert_staticcall_cases(vec![StaticcallCase::word(
            "symbol",
            IB20::symbolCall {}.abi_encode(),
            U256::from(32),
        )])
        .await;

    scenario.derive().await;
}

/// Verifies that the decimals() view returns the stablecoin fixed precision.
#[tokio::test]
async fn stablecoin_decimals_returns_correct_value() {
    let mut scenario = B20StablecoinScenario::new().await;

    scenario
        .assert_staticcall_cases(vec![StaticcallCase::word(
            "decimals",
            IB20::decimalsCall {}.abi_encode(),
            U256::from(STABLECOIN_TOKEN_DECIMALS),
        )])
        .await;

    scenario.derive().await;
}

/// Verifies that updateContractURI persists and is readable via contractURI.
#[tokio::test]
async fn stablecoin_update_contract_uri_persists() {
    let mut scenario = B20StablecoinScenario::new().await;

    let grant_metadata = scenario.call_tx(IB20::grantRoleCall {
        role: B20TokenRole::Metadata.id(),
        account: BerylTestEnv::alice(),
    });
    let block = scenario.build_block(vec![grant_metadata]).await;
    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "METADATA_ROLE grant must succeed before updating contract URI"
    );

    let update_uri =
        scenario.call_tx(IB20::updateContractURICall { newURI: "https://example.com".to_string() });
    let block = scenario.build_block(vec![update_uri]).await;
    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "updateContractURI must succeed for alice with METADATA_ROLE"
    );

    scenario
        .assert_staticcall_cases(vec![StaticcallCase::word(
            "contractURI",
            IB20::contractURICall {}.abi_encode(),
            U256::from(32),
        )])
        .await;

    scenario.derive().await;
}

/// Verifies that granting MINT_ROLE allows minting and revoking it blocks minting.
#[tokio::test]
async fn stablecoin_grant_and_revoke_role() {
    let mut scenario = B20StablecoinScenario::new().await;

    let grant_mint_to_bob = scenario.call_tx(IB20::grantRoleCall {
        role: B20TokenRole::Mint.id(),
        account: BerylTestEnv::bob(),
    });
    let block = scenario.build_block(vec![grant_mint_to_bob]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "MINT_ROLE grant to bob must succeed");

    const MINT_AMOUNT: u64 = 100;
    let bob_mint = scenario
        .bob_call_tx(IB20::mintCall { to: BerylTestEnv::bob(), amount: U256::from(MINT_AMOUNT) });
    let block = scenario.build_block(vec![bob_mint]).await;
    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "bob with MINT_ROLE must be able to mint tokens"
    );

    let revoke_mint_from_bob = scenario.call_tx(IB20::revokeRoleCall {
        role: B20TokenRole::Mint.id(),
        account: BerylTestEnv::bob(),
    });
    let block = scenario.build_block(vec![revoke_mint_from_bob]).await;
    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "MINT_ROLE revocation from bob must succeed"
    );

    let bob_mint_blocked = scenario
        .bob_call_tx(IB20::mintCall { to: BerylTestEnv::bob(), amount: U256::from(MINT_AMOUNT) });
    let block = scenario.build_block(vec![bob_mint_blocked]).await;
    assert!(
        !scenario.env.user_tx_succeeded(&block, 0),
        "bob without MINT_ROLE must not be able to mint tokens"
    );

    scenario.derive().await;
}

/// Verifies that pausing the TRANSFER feature blocks transfers and unpausing restores them.
#[tokio::test]
async fn stablecoin_pause_blocks_transfer_and_unpause_restores() {
    let mut scenario = B20StablecoinScenario::new().await;

    let grant_pause = scenario.call_tx(IB20::grantRoleCall {
        role: B20TokenRole::Pause.id(),
        account: BerylTestEnv::alice(),
    });
    let grant_unpause = scenario.call_tx(IB20::grantRoleCall {
        role: B20TokenRole::Unpause.id(),
        account: BerylTestEnv::alice(),
    });
    let block = scenario.build_block(vec![grant_pause, grant_unpause]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "PAUSE_ROLE grant must succeed");
    assert!(scenario.env.user_tx_succeeded(&block, 1), "UNPAUSE_ROLE grant must succeed");

    let pause =
        scenario.call_tx(IB20::pauseCall { features: vec![IB20::PausableFeature::TRANSFER] });
    let block = scenario.build_block(vec![pause]).await;
    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "pause must succeed for alice with PAUSE_ROLE"
    );

    let transfer_paused =
        scenario.call_tx(IB20::transferCall { to: BerylTestEnv::bob(), amount: U256::ONE });
    let block = scenario.build_block(vec![transfer_paused]).await;
    assert!(
        !scenario.env.user_tx_succeeded(&block, 0),
        "transfer must revert while the TRANSFER feature is paused"
    );

    let unpause =
        scenario.call_tx(IB20::unpauseCall { features: vec![IB20::PausableFeature::TRANSFER] });
    let block = scenario.build_block(vec![unpause]).await;
    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "unpause must succeed for alice with UNPAUSE_ROLE"
    );

    let transfer_unpaused =
        scenario.call_tx(IB20::transferCall { to: BerylTestEnv::bob(), amount: U256::ONE });
    let block = scenario.build_block(vec![transfer_unpaused]).await;
    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "transfer must succeed after the TRANSFER feature is unpaused"
    );

    scenario.derive().await;
}

/// Verifies that transferWithMemo emits a Transfer event and updates balances.
#[tokio::test]
async fn stablecoin_transfer_with_memo_emits_transfer_event() {
    let mut scenario = B20StablecoinScenario::new().await;

    const TRANSFER_AMOUNT: u64 = 100;
    let transfer_with_memo = scenario.call_tx(IB20::transferWithMemoCall {
        to: BerylTestEnv::bob(),
        amount: U256::from(TRANSFER_AMOUNT),
        memo: STABLECOIN_MEMO,
    });
    let block = scenario.build_block(vec![transfer_with_memo]).await;

    assert!(scenario.env.user_tx_succeeded(&block, 0), "transferWithMemo must succeed");
    assert!(
        scenario.env.b20_transfer_log_emitted(
            &block,
            0,
            scenario.token,
            BerylTestEnv::alice(),
            BerylTestEnv::bob(),
            U256::from(TRANSFER_AMOUNT),
        ),
        "transferWithMemo must emit a Transfer event"
    );
    scenario
        .assert_balance(BerylTestEnv::alice(), BerylTestEnv::B20_INITIAL_SUPPLY - TRANSFER_AMOUNT);
    scenario.assert_balance(BerylTestEnv::bob(), TRANSFER_AMOUNT);

    scenario.derive().await;
}

/// Test harness for stablecoin B-20 scenarios.
///
/// Activates `B20Factory` and `B20Stablecoin` together, creates a stablecoin token with
/// an initial supply minted to Alice, and accumulates blocks so they can all be derived
/// together at the end of the test.
struct B20StablecoinScenario {
    env: BerylTestEnv,
    token: Address,
    creation_block: BaseBlock,
    blocks: Vec<(BaseBlock, u64)>,
}

impl B20StablecoinScenario {
    async fn new() -> Self {
        let env = BerylTestEnv::new();
        let token =
            B20Variant::Stablecoin.compute_address(BerylTestEnv::alice(), STABLECOIN_SALT).0;
        let mut scenario =
            Self { env, token, creation_block: BaseBlock::default(), blocks: Vec::new() };

        scenario.build_block(vec![]).await;

        let activate_factory = scenario.env.activate_feature_tx(ActivationFeature::B20Factory.id());
        let activate_stablecoin =
            scenario.env.activate_feature_tx(ActivationFeature::B20Stablecoin.id());
        let block = scenario.build_block(vec![activate_factory, activate_stablecoin]).await;
        assert!(scenario.env.user_tx_succeeded(&block, 0), "B20_FACTORY activation must succeed");
        assert!(
            scenario.env.user_tx_succeeded(&block, 1),
            "B20_STABLECOIN activation must succeed"
        );

        let create = scenario.env.create_tx(
            TxKind::Call(B20FactoryStorage::ADDRESS),
            Bytes::from(
                IB20Factory::createB20Call {
                    variant: IB20Factory::B20Variant::STABLECOIN,
                    salt: STABLECOIN_SALT,
                    params: IB20Factory::B20StablecoinCreateParams {
                        version: B20FactoryStorage::CREATE_TOKEN_VERSION,
                        name: STABLECOIN_NAME.to_string(),
                        symbol: STABLECOIN_SYMBOL.to_string(),
                        initialAdmin: BerylTestEnv::alice(),
                        currency: STABLECOIN_CURRENCY.to_string(),
                    }
                    .abi_encode()
                    .into(),
                    initCalls: vec![
                        IB20::mintCall {
                            to: BerylTestEnv::alice(),
                            amount: U256::from(BerylTestEnv::B20_INITIAL_SUPPLY),
                        }
                        .abi_encode()
                        .into(),
                    ],
                }
                .abi_encode(),
            ),
            BerylTestEnv::B20_GAS_LIMIT,
        );
        let creation_block = scenario.build_block(vec![create]).await;

        assert!(
            scenario.env.user_tx_succeeded(&creation_block, 0),
            "stablecoin creation transaction must succeed"
        );
        assert!(
            scenario.env.sequencer.has_code(token),
            "stablecoin token code must be deployed after creation"
        );
        scenario.assert_total_supply(BerylTestEnv::B20_INITIAL_SUPPLY);
        scenario.assert_balance(BerylTestEnv::alice(), BerylTestEnv::B20_INITIAL_SUPPLY);

        scenario.creation_block = creation_block;
        scenario
    }

    async fn build_block(&mut self, txs: Vec<BaseTxEnvelope>) -> BaseBlock {
        let block = self.env.sequencer.build_next_block_with_transactions(txs).await;
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

        let deploy_block = self.build_block(deployments).await;
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
        let call_block = self.build_block(calls).await;
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
        }
    }

    fn assert_total_supply(&self, total_supply: u64) {
        assert_eq!(
            self.env.b20_total_supply(self.token),
            U256::from(total_supply),
            "stablecoin total supply must match the expected value"
        );
    }

    fn assert_balance(&self, account: Address, expected: u64) {
        assert_eq!(
            self.env.b20_balance(self.token, account),
            U256::from(expected),
            "stablecoin balance for {account} must match the expected value"
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
}

impl StaticcallCase {
    const fn word(label: &'static str, input: Vec<u8>, expected_word: U256) -> Self {
        Self { label, input, expected_word: Some(expected_word) }
    }
}

fn word_from_address(address: Address) -> U256 {
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(address.as_slice());
    U256::from_be_slice(&word)
}
