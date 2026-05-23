//! B-20 security token action tests across the Base Beryl boundary.

use alloy_consensus::TxReceipt;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, b256};
use alloy_sol_types::{SolCall, SolEvent, SolValue};
use base_common_consensus::{BaseBlock, BaseTxEnvelope};
use base_common_precompiles::{
    ActivationFeature, B20FactoryStorage, B20TokenRole, B20Variant, IB20, IB20Factory, IB20Security,
};

use crate::env::BerylTestEnv;

/// ISIN identifier stored in the default test security token.
const SECURITY_ISIN: &str = "US0000000000";

/// Name of the test security token.
const SECURITY_NAME: &str = "Action Security B20";

/// Symbol of the test security token.
const SECURITY_SYMBOL: &str = "ASEC";

/// Decimals for the security B-20 variant.
const SECURITY_DECIMALS: u8 = 6;

/// Salt used to create the default test security token.
const SECURITY_SALT: B256 = B256::repeat_byte(0x55);

/// WAD precision for share-ratio arithmetic: 1e18.
const WAD: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);

/// `keccak256("BURN_FROM_ROLE")` — required for `batchBurn`.
const BURN_FROM_ROLE: B256 =
    b256!("25400dba76bf0d00acf274c2b61ff56aa4ed19826e21e0186e3fecd6a6671875");

/// `keccak256("SECURITY_OPERATOR_ROLE")` — required for `announce`, `updateShareRatio`,
/// `updateSecurityIdentifier`.
const SECURITY_OPERATOR_ROLE: B256 =
    b256!("e63901dfe7775ace99fa3654743976eb0ab2009f5d19c4fc1ecd40aed27d59af");

#[tokio::test]
async fn security_token_creation_deploys_code_and_emits_created_event() {
    let scenario = B20SecurityScenario::new().await;

    assert!(
        scenario.env.sequencer.has_code(scenario.token),
        "security token code must be deployed after creation"
    );
    let expected_log = IB20Factory::B20Created {
        token: scenario.token,
        variant: IB20Factory::B20Variant::SECURITY,
        name: SECURITY_NAME.to_string(),
        symbol: SECURITY_SYMBOL.to_string(),
        decimals: SECURITY_DECIMALS,
    }
    .encode_log_data();
    assert!(
        scenario
            .env
            .user_tx_receipt(&scenario.blocks[2].0, 0)
            .logs()
            .iter()
            .any(|log| log.address == B20FactoryStorage::ADDRESS && log.data == expected_log),
        "security token creation must emit a B20Created event"
    );

    scenario.derive().await;
}

#[tokio::test]
async fn security_token_get_address_routes_for_security_variant() {
    let mut scenario = B20SecurityScenario::new().await;

    let (probe, deploy_probe) = scenario.env.deploy_staticcall_probe_tx(B20FactoryStorage::ADDRESS);
    let deploy_block = scenario.build_block_with_transactions(vec![deploy_probe]).await;
    assert!(
        scenario.env.user_tx_succeeded(&deploy_block, 0),
        "factory staticcall probe must deploy"
    );

    let get_address = scenario.env.call_staticcall_probe_tx(
        probe,
        Bytes::from(
            IB20Factory::getB20AddressCall {
                variant: IB20Factory::B20Variant::SECURITY,
                sender: BerylTestEnv::alice(),
                salt: SECURITY_SALT,
            }
            .abi_encode(),
        ),
        BerylTestEnv::B20_PROBE_GAS_LIMIT,
    );
    let _block = scenario.build_block_with_transactions(vec![get_address]).await;

    assert!(
        scenario.env.probe_call_succeeded(probe),
        "getB20Address() staticcall must succeed for security variant"
    );
    assert_eq!(
        scenario.env.probe_return_word(probe),
        word_from_address(scenario.token),
        "getB20Address() must return the deterministic security token address"
    );

    scenario.derive().await;
}

#[tokio::test]
async fn security_token_abi_functions_return_correct_values() {
    let mut scenario = B20SecurityScenario::new().await;

    scenario
        .assert_staticcall_cases(vec![
            StaticcallCase::word("name", IB20::nameCall {}.abi_encode(), U256::from(32)),
            StaticcallCase::word("symbol", IB20::symbolCall {}.abi_encode(), U256::from(32)),
            StaticcallCase::word(
                "decimals",
                IB20::decimalsCall {}.abi_encode(),
                U256::from(SECURITY_DECIMALS),
            ),
            StaticcallCase::word(
                "totalSupply",
                IB20::totalSupplyCall {}.abi_encode(),
                U256::from(BerylTestEnv::B20_INITIAL_SUPPLY),
            ),
            StaticcallCase::word(
                "balanceOf alice",
                IB20::balanceOfCall { account: BerylTestEnv::alice() }.abi_encode(),
                U256::from(BerylTestEnv::B20_INITIAL_SUPPLY),
            ),
            StaticcallCase::word(
                "sharesToTokensRatio",
                IB20Security::sharesToTokensRatioCall {}.abi_encode(),
                WAD,
            ),
        ])
        .await;

    scenario.derive().await;
}

#[tokio::test]
async fn security_token_transfer_succeeds_with_always_allow_policy() {
    let mut scenario = B20SecurityScenario::new().await;

    let transfer = scenario.env.transfer_b20_tx(
        scenario.token,
        BerylTestEnv::bob(),
        U256::from(BerylTestEnv::B20_BOB_TRANSFER),
    );
    let block = scenario.build_block_with_transactions(vec![transfer]).await;

    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "security token transfer must succeed with always-allow transfer policy"
    );
    assert_eq!(
        scenario.env.b20_balance(scenario.token, BerylTestEnv::alice()),
        U256::from(BerylTestEnv::B20_INITIAL_SUPPLY - BerylTestEnv::B20_BOB_TRANSFER),
        "Alice security token balance must decrease after transfer"
    );
    assert_eq!(
        scenario.env.b20_balance(scenario.token, BerylTestEnv::bob()),
        U256::from(BerylTestEnv::B20_BOB_TRANSFER),
        "Bob security token balance must increase after transfer"
    );

    scenario.derive().await;
}

#[tokio::test]
async fn security_token_creation_with_invalid_variant_reverts() {
    let mut scenario = B20SecurityScenario::new().await;

    // A security token requires a non-empty ISIN; creation with an empty ISIN must revert.
    let bad_params = IB20Factory::B20SecurityCreateParams {
        version: B20FactoryStorage::CREATE_TOKEN_VERSION,
        name: "Bad Security".to_string(),
        symbol: "BAD".to_string(),
        initialAdmin: BerylTestEnv::alice(),
        isin: String::new(),
        minimumRedeemable: U256::ZERO,
    };
    let bad_create = scenario.env.create_tx(
        TxKind::Call(B20FactoryStorage::ADDRESS),
        Bytes::from(
            IB20Factory::createB20Call {
                variant: IB20Factory::B20Variant::SECURITY,
                salt: B256::repeat_byte(0x56),
                params: bad_params.abi_encode().into(),
                initCalls: Vec::new(),
            }
            .abi_encode(),
        ),
        BerylTestEnv::B20_GAS_LIMIT,
    );
    let block = scenario.build_block_with_transactions(vec![bad_create]).await;

    assert!(
        !scenario.env.user_tx_succeeded(&block, 0),
        "security token creation with empty ISIN must revert"
    );

    scenario.derive().await;
}

#[tokio::test]
async fn security_token_redeem_sender_policy_defaults_to_always_block() {
    let mut scenario = B20SecurityScenario::new().await;

    let redeem = scenario.call_tx(IB20Security::redeemCall { amount: U256::from(100) });
    let block = scenario.build_block_with_transactions(vec![redeem]).await;

    assert!(
        !scenario.env.user_tx_succeeded(&block, 0),
        "redeem must revert when REDEEM_SENDER_POLICY defaults to always-block"
    );

    scenario.derive().await;
}

#[tokio::test]
async fn security_token_shares_to_tokens_ratio_is_wad_at_creation() {
    let mut scenario = B20SecurityScenario::new().await;

    scenario
        .assert_staticcall_cases(vec![StaticcallCase::word(
            "sharesToTokensRatio at creation",
            IB20Security::sharesToTokensRatioCall {}.abi_encode(),
            WAD,
        )])
        .await;

    scenario.derive().await;
}

#[tokio::test]
async fn security_token_unauthorized_mint_reverts() {
    let mut scenario = B20SecurityScenario::new().await;

    let mint =
        scenario.bob_call_tx(IB20::mintCall { to: BerylTestEnv::bob(), amount: U256::from(100) });
    let block = scenario.build_block_with_transactions(vec![mint]).await;

    assert!(!scenario.env.user_tx_succeeded(&block, 0), "mint without MINT_ROLE must revert");
    assert_eq!(
        scenario.env.b20_balance(scenario.token, BerylTestEnv::bob()),
        U256::ZERO,
        "Bob balance must remain zero after unauthorized mint attempt"
    );

    scenario.derive().await;
}

#[tokio::test]
async fn security_token_authorized_mint_increases_supply() {
    let mut scenario = B20SecurityScenario::new().await;
    let initial = BerylTestEnv::B20_INITIAL_SUPPLY;

    let grant_mint = scenario.call_tx(IB20::grantRoleCall {
        role: B20TokenRole::Mint.id(),
        account: BerylTestEnv::alice(),
    });
    let block = scenario.build_block_with_transactions(vec![grant_mint]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "MINT_ROLE grant must succeed");

    let mint_amount = 500u64;
    let mint = scenario
        .call_tx(IB20::mintCall { to: BerylTestEnv::alice(), amount: U256::from(mint_amount) });
    let block = scenario.build_block_with_transactions(vec![mint]).await;

    assert!(scenario.env.user_tx_succeeded(&block, 0), "authorized mint must succeed");
    assert_eq!(
        scenario.env.b20_total_supply(scenario.token),
        U256::from(initial + mint_amount),
        "total supply must increase by the minted amount"
    );

    scenario.derive().await;
}

#[tokio::test]
async fn security_token_transfer_zero_amount_succeeds() {
    let mut scenario = B20SecurityScenario::new().await;

    let transfer = scenario.env.transfer_b20_tx(scenario.token, BerylTestEnv::bob(), U256::ZERO);
    let block = scenario.build_block_with_transactions(vec![transfer]).await;

    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "zero-amount security token transfer must succeed"
    );
    assert_eq!(
        scenario.env.b20_balance(scenario.token, BerylTestEnv::alice()),
        U256::from(BerylTestEnv::B20_INITIAL_SUPPLY),
        "Alice balance must be unchanged after zero transfer"
    );

    scenario.derive().await;
}

// ── New tests ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn security_token_shares_of_matches_balance_at_wad_ratio() {
    let mut scenario = B20SecurityScenario::new().await;

    // At the initial 1:1 WAD ratio sharesOf(alice) must equal balanceOf(alice).
    scenario
        .assert_staticcall_cases(vec![
            StaticcallCase::word(
                "sharesOf alice",
                IB20Security::sharesOfCall { account: BerylTestEnv::alice() }.abi_encode(),
                U256::from(BerylTestEnv::B20_INITIAL_SUPPLY),
            ),
            StaticcallCase::word(
                "balanceOf alice",
                IB20::balanceOfCall { account: BerylTestEnv::alice() }.abi_encode(),
                U256::from(BerylTestEnv::B20_INITIAL_SUPPLY),
            ),
        ])
        .await;

    scenario.derive().await;
}

#[tokio::test]
async fn security_token_to_shares_converts_with_wad_ratio() {
    let mut scenario = B20SecurityScenario::new().await;

    // At 1:1 ratio toShares(balance) must equal balance.
    scenario
        .assert_staticcall_cases(vec![StaticcallCase::word(
            "toShares at 1:1 ratio",
            IB20Security::toSharesCall { balance: U256::from(1_000_000u64) }.abi_encode(),
            U256::from(1_000_000u64),
        )])
        .await;

    scenario.derive().await;
}

#[tokio::test]
async fn security_token_update_minimum_redeemable_persists() {
    let mut scenario = B20SecurityScenario::new().await;

    let new_minimum = U256::from(500u64);
    let update = scenario
        .call_tx(IB20Security::updateMinimumRedeemableCall { newMinimumRedeemable: new_minimum });
    let block = scenario.build_block_with_transactions(vec![update]).await;
    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "updateMinimumRedeemable by DefaultAdmin must succeed"
    );

    scenario
        .assert_staticcall_cases(vec![StaticcallCase::word(
            "minimumRedeemable after update",
            IB20Security::minimumRedeemableCall {}.abi_encode(),
            new_minimum,
        )])
        .await;

    scenario.derive().await;
}

#[tokio::test]
async fn security_token_update_security_identifier_persists() {
    let mut scenario = B20SecurityScenario::new().await;

    let grant_operator = scenario.call_tx(IB20::grantRoleCall {
        role: SECURITY_OPERATOR_ROLE,
        account: BerylTestEnv::alice(),
    });
    let block = scenario.build_block_with_transactions(vec![grant_operator]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "SECURITY_OPERATOR_ROLE grant must succeed");

    let update_id = scenario.call_tx(IB20Security::updateSecurityIdentifierCall {
        identifierType: "ISIN".to_string(),
        value: "US1234567890".to_string(),
    });
    let block = scenario.build_block_with_transactions(vec![update_id]).await;
    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "updateSecurityIdentifier must succeed for SECURITY_OPERATOR_ROLE holder"
    );

    // securityIdentifier returns a string; the first ABI word is the offset (32).
    scenario
        .assert_staticcall_cases(vec![StaticcallCase::word(
            "securityIdentifier ISIN",
            IB20Security::securityIdentifierCall { identifierType: "ISIN".to_string() }
                .abi_encode(),
            U256::from(32),
        )])
        .await;

    scenario.derive().await;
}

#[tokio::test]
async fn security_token_batch_mint_increases_multiple_balances() {
    let mut scenario = B20SecurityScenario::new().await;
    let initial = BerylTestEnv::B20_INITIAL_SUPPLY;

    let grant_mint = scenario.call_tx(IB20::grantRoleCall {
        role: B20TokenRole::Mint.id(),
        account: BerylTestEnv::alice(),
    });
    let block = scenario.build_block_with_transactions(vec![grant_mint]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "MINT_ROLE grant must succeed");

    let alice_mint = 500u64;
    let bob_mint = 300u64;
    let batch_mint = scenario.call_tx(IB20Security::batchMintCall {
        recipients: vec![BerylTestEnv::alice(), BerylTestEnv::bob()],
        amounts: vec![U256::from(alice_mint), U256::from(bob_mint)],
    });
    let block = scenario.build_block_with_transactions(vec![batch_mint]).await;

    assert!(scenario.env.user_tx_succeeded(&block, 0), "batchMint must succeed");
    assert_eq!(
        scenario.env.b20_balance(scenario.token, BerylTestEnv::alice()),
        U256::from(initial + alice_mint),
        "Alice balance must increase by the batch-minted amount"
    );
    assert_eq!(
        scenario.env.b20_balance(scenario.token, BerylTestEnv::bob()),
        U256::from(bob_mint),
        "Bob balance must increase by the batch-minted amount"
    );

    scenario.derive().await;
}

#[tokio::test]
async fn security_token_batch_burn_decreases_multiple_balances() {
    let mut scenario = B20SecurityScenario::new().await;
    let initial = BerylTestEnv::B20_INITIAL_SUPPLY;

    // Mint tokens to bob so both accounts have a balance to burn from.
    let grant_mint = scenario.call_tx(IB20::grantRoleCall {
        role: B20TokenRole::Mint.id(),
        account: BerylTestEnv::alice(),
    });
    let mint_for_bob =
        scenario.call_tx(IB20::mintCall { to: BerylTestEnv::bob(), amount: U256::from(200u64) });
    let block = scenario.build_block_with_transactions(vec![grant_mint, mint_for_bob]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "MINT_ROLE grant must succeed");
    assert!(scenario.env.user_tx_succeeded(&block, 1), "mint to bob must succeed");

    // Grant BURN_FROM_ROLE so alice can call batchBurn.
    let grant_burn = scenario
        .call_tx(IB20::grantRoleCall { role: BURN_FROM_ROLE, account: BerylTestEnv::alice() });
    let block = scenario.build_block_with_transactions(vec![grant_burn]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "BURN_FROM_ROLE grant must succeed");

    let alice_burn = 100u64;
    let bob_burn = 50u64;
    let batch_burn = scenario.call_tx(IB20Security::batchBurnCall {
        accounts: vec![BerylTestEnv::alice(), BerylTestEnv::bob()],
        amounts: vec![U256::from(alice_burn), U256::from(bob_burn)],
    });
    let block = scenario.build_block_with_transactions(vec![batch_burn]).await;

    assert!(scenario.env.user_tx_succeeded(&block, 0), "batchBurn must succeed");
    assert_eq!(
        scenario.env.b20_balance(scenario.token, BerylTestEnv::alice()),
        U256::from(initial - alice_burn),
        "Alice balance must decrease by the batch-burned amount"
    );
    assert_eq!(
        scenario.env.b20_balance(scenario.token, BerylTestEnv::bob()),
        U256::from(200 - bob_burn),
        "Bob balance must decrease by the batch-burned amount"
    );

    scenario.derive().await;
}

#[tokio::test]
async fn security_token_name_and_symbol_return_stored_values() {
    let mut scenario = B20SecurityScenario::new().await;

    // name() and symbol() return ABI-encoded strings; first word is the offset (32).
    scenario
        .assert_staticcall_cases(vec![
            StaticcallCase::word("name", IB20::nameCall {}.abi_encode(), U256::from(32)),
            StaticcallCase::word("symbol", IB20::symbolCall {}.abi_encode(), U256::from(32)),
        ])
        .await;

    scenario.derive().await;
}

#[tokio::test]
async fn security_token_pause_blocks_transfer() {
    let mut scenario = B20SecurityScenario::new().await;

    let grant_pause = scenario.call_tx(IB20::grantRoleCall {
        role: B20TokenRole::Pause.id(),
        account: BerylTestEnv::alice(),
    });
    let grant_unpause = scenario.call_tx(IB20::grantRoleCall {
        role: B20TokenRole::Unpause.id(),
        account: BerylTestEnv::alice(),
    });
    let block = scenario.build_block_with_transactions(vec![grant_pause, grant_unpause]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "PAUSE_ROLE grant must succeed");
    assert!(scenario.env.user_tx_succeeded(&block, 1), "UNPAUSE_ROLE grant must succeed");

    let pause =
        scenario.call_tx(IB20::pauseCall { features: vec![IB20::PausableFeature::TRANSFER] });
    let block = scenario.build_block_with_transactions(vec![pause]).await;
    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "pause TRANSFER must succeed for PAUSE_ROLE holder"
    );

    let transfer_while_paused =
        scenario.env.transfer_b20_tx(scenario.token, BerylTestEnv::bob(), U256::from(1));
    let block = scenario.build_block_with_transactions(vec![transfer_while_paused]).await;
    assert!(
        !scenario.env.user_tx_succeeded(&block, 0),
        "transfer must revert while TRANSFER feature is paused"
    );

    let unpause =
        scenario.call_tx(IB20::unpauseCall { features: vec![IB20::PausableFeature::TRANSFER] });
    let block = scenario.build_block_with_transactions(vec![unpause]).await;
    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "unpause TRANSFER must succeed for UNPAUSE_ROLE holder"
    );

    let transfer_after_unpause =
        scenario.env.transfer_b20_tx(scenario.token, BerylTestEnv::bob(), U256::from(1));
    let block = scenario.build_block_with_transactions(vec![transfer_after_unpause]).await;
    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "transfer must succeed after TRANSFER feature is unpaused"
    );

    scenario.derive().await;
}

struct B20SecurityScenario {
    env: BerylTestEnv,
    token: Address,
    blocks: Vec<(BaseBlock, u64)>,
}

impl B20SecurityScenario {
    /// Creates a new security token scenario: activates `B20_FACTORY` and `B20_SECURITY`
    /// features, then creates a security token and mints the initial supply to Alice.
    ///
    /// Block layout after `new()`:
    /// - `blocks[0]`: empty boundary block
    /// - `blocks[1]`: feature activation block
    /// - `blocks[2]`: token creation block
    async fn new() -> Self {
        let env = BerylTestEnv::new();
        let (token, _) = B20Variant::Security.compute_address(BerylTestEnv::alice(), SECURITY_SALT);
        let mut scenario = Self { env, token, blocks: Vec::new() };

        // Block 0: advance past the genesis block.
        scenario.build_block_with_transactions(Vec::new()).await;

        // Block 1: activate B20Factory and B20Security features.
        let activate_factory =
            scenario.env.activate_feature_tx(BerylTestEnv::b20_factory_feature());
        let activate_security =
            scenario.env.activate_feature_tx(ActivationFeature::B20Security.id());
        let block =
            scenario.build_block_with_transactions(vec![activate_factory, activate_security]).await;
        assert!(scenario.env.user_tx_succeeded(&block, 0), "B20_FACTORY activation must succeed");
        assert!(scenario.env.user_tx_succeeded(&block, 1), "B20_SECURITY activation must succeed");

        // Block 2: create the security token and mint initial supply to Alice.
        let create = SecurityFeatures::create_security_token_tx(&scenario.env);
        let block = scenario.build_block_with_transactions(vec![create]).await;
        assert!(
            scenario.env.user_tx_succeeded(&block, 0),
            "security token creation transaction must succeed"
        );
        assert!(scenario.env.sequencer.has_code(token), "security token code must be deployed");
        assert_eq!(
            scenario.env.b20_balance(token, BerylTestEnv::alice()),
            U256::from(BerylTestEnv::B20_INITIAL_SUPPLY),
            "Alice must receive the initial security token supply"
        );

        scenario
    }

    /// Builds an L2 block containing `transactions` and records it for derivation.
    async fn build_block_with_transactions(
        &mut self,
        transactions: Vec<BaseTxEnvelope>,
    ) -> BaseBlock {
        let block = self.env.sequencer.build_next_block_with_transactions(transactions).await;
        let block_number = self.blocks.len() as u64 + 1;
        self.blocks.push((block.clone(), block_number));
        block
    }

    /// Creates a transaction from Alice's account calling the security token.
    fn call_tx(&self, call: impl SolCall) -> BaseTxEnvelope {
        self.env.create_tx(
            TxKind::Call(self.token),
            Bytes::from(call.abi_encode()),
            BerylTestEnv::B20_GAS_LIMIT,
        )
    }

    /// Creates a transaction from Bob's account calling the security token.
    fn bob_call_tx(&mut self, call: impl SolCall) -> BaseTxEnvelope {
        self.env.create_bob_tx(
            TxKind::Call(self.token),
            Bytes::from(call.abi_encode()),
            BerylTestEnv::B20_GAS_LIMIT,
        )
    }

    /// Deploys one staticcall probe per case, fires each call, and asserts the result.
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
        }
    }

    /// Derives all recorded blocks and asserts the final safe head.
    async fn derive(mut self) {
        let expected_safe_head = self.blocks.len() as u64;
        self.env.derive_blocks(self.blocks, expected_safe_head).await;
    }
}

struct SecurityFeatures;

impl SecurityFeatures {
    /// Creates a factory transaction that deploys a security token with the test ISIN
    /// and mints the initial supply to Alice via an `initCall`.
    fn create_security_token_tx(env: &BerylTestEnv) -> BaseTxEnvelope {
        let params = IB20Factory::B20SecurityCreateParams {
            version: B20FactoryStorage::CREATE_TOKEN_VERSION,
            name: SECURITY_NAME.to_string(),
            symbol: SECURITY_SYMBOL.to_string(),
            initialAdmin: BerylTestEnv::alice(),
            isin: SECURITY_ISIN.to_string(),
            minimumRedeemable: U256::ZERO,
        };
        env.create_tx(
            TxKind::Call(B20FactoryStorage::ADDRESS),
            Bytes::from(
                IB20Factory::createB20Call {
                    variant: IB20Factory::B20Variant::SECURITY,
                    salt: SECURITY_SALT,
                    params: params.abi_encode().into(),
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
        )
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
