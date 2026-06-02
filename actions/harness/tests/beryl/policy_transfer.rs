//! Policy-gated B-20 transfer action tests across the Base Beryl boundary.
//!
//! These tests verify the cross-precompile integration between the B-20 token precompile and
//! the `PolicyRegistry` precompile: every transfer call checks the sender against the token's
//! configured `TRANSFER_SENDER_POLICY`, and the result drives allow/block decisions end-to-end.

use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_sol_types::{SolCall, SolValue};
use base_common_consensus::{BaseBlock, BaseTxEnvelope};
use base_common_precompiles::{
    B20FactoryStorage, B20PolicyType, B20Variant, IB20, IB20Factory, IPolicyRegistry,
    PolicyRegistryStorage,
};

use crate::env::BerylTestEnv;

const GAS_LIMIT: u64 = 10_000_000;

/// Transfer amount used across all policy gating tests.
const TRANSFER_AMOUNT: u64 = 1_000;

// --- ALLOWLIST ---

#[tokio::test]
async fn allowlist_policy_gates_b20_transfers() {
    let allowlist_id = BerylTestEnv::policy_id(IPolicyRegistry::PolicyType::ALLOWLIST, 2);
    let mut scenario = PolicyTransferScenario::new_with_custom_policy(
        IPolicyRegistry::PolicyType::ALLOWLIST,
        allowlist_id,
    )
    .await;

    // Non-member: transfer must revert because Alice is not yet in the allowlist.
    let blocked = scenario.env.transfer_b20_tx(
        scenario.token,
        BerylTestEnv::bob(),
        U256::from(TRANSFER_AMOUNT),
    );
    let block = scenario.build_block_with_transactions(vec![blocked]).await;
    assert!(
        !scenario.env.user_tx_succeeded(&block, 0),
        "transfer from non-allowlist member must revert"
    );
    scenario.assert_balance(BerylTestEnv::alice(), BerylTestEnv::B20_INITIAL_SUPPLY);
    scenario.assert_balance(BerylTestEnv::bob(), 0);

    // Add Alice to the allowlist.
    let add_alice = scenario.policy_tx(IPolicyRegistry::updateAllowlistCall {
        policyId: allowlist_id,
        allowed: true,
        accounts: vec![BerylTestEnv::alice()],
    });
    let block = scenario.build_block_with_transactions(vec![add_alice]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "updateAllowlist() must succeed");

    // Allowlist member: transfer must succeed.
    let allowed = scenario.env.transfer_b20_tx(
        scenario.token,
        BerylTestEnv::bob(),
        U256::from(TRANSFER_AMOUNT),
    );
    let block = scenario.build_block_with_transactions(vec![allowed]).await;
    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "transfer from allowlist member must succeed"
    );
    scenario
        .assert_balance(BerylTestEnv::alice(), BerylTestEnv::B20_INITIAL_SUPPLY - TRANSFER_AMOUNT);
    scenario.assert_balance(BerylTestEnv::bob(), TRANSFER_AMOUNT);

    // Remove Alice from the allowlist.
    let remove_alice = scenario.policy_tx(IPolicyRegistry::updateAllowlistCall {
        policyId: allowlist_id,
        allowed: false,
        accounts: vec![BerylTestEnv::alice()],
    });
    let block = scenario.build_block_with_transactions(vec![remove_alice]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "updateAllowlist(remove) must succeed");

    // Re-blocked: transfer must revert once Alice is removed from the allowlist.
    let re_blocked = scenario.env.transfer_b20_tx(
        scenario.token,
        BerylTestEnv::bob(),
        U256::from(TRANSFER_AMOUNT),
    );
    let block = scenario.build_block_with_transactions(vec![re_blocked]).await;
    assert!(
        !scenario.env.user_tx_succeeded(&block, 0),
        "transfer from removed allowlist member must revert"
    );

    scenario.derive().await;
}

// --- BLOCKLIST ---

#[tokio::test]
async fn blocklist_policy_gates_b20_transfers() {
    let blocklist_id = BerylTestEnv::policy_id(IPolicyRegistry::PolicyType::BLOCKLIST, 2);
    let mut scenario = PolicyTransferScenario::new_with_custom_policy(
        IPolicyRegistry::PolicyType::BLOCKLIST,
        blocklist_id,
    )
    .await;

    // Non-blocked sender: transfer must succeed.
    let allowed = scenario.env.transfer_b20_tx(
        scenario.token,
        BerylTestEnv::bob(),
        U256::from(TRANSFER_AMOUNT),
    );
    let block = scenario.build_block_with_transactions(vec![allowed]).await;
    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "transfer from non-blocklisted sender must succeed"
    );
    scenario
        .assert_balance(BerylTestEnv::alice(), BerylTestEnv::B20_INITIAL_SUPPLY - TRANSFER_AMOUNT);
    scenario.assert_balance(BerylTestEnv::bob(), TRANSFER_AMOUNT);

    // Block Alice.
    let block_alice = scenario.policy_tx(IPolicyRegistry::updateBlocklistCall {
        policyId: blocklist_id,
        blocked: true,
        accounts: vec![BerylTestEnv::alice()],
    });
    let block = scenario.build_block_with_transactions(vec![block_alice]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "updateBlocklist() must succeed");

    // Blocked sender: transfer must revert.
    let blocked = scenario.env.transfer_b20_tx(
        scenario.token,
        BerylTestEnv::carol(),
        U256::from(TRANSFER_AMOUNT),
    );
    let block = scenario.build_block_with_transactions(vec![blocked]).await;
    assert!(!scenario.env.user_tx_succeeded(&block, 0), "transfer from blocked sender must revert");
    scenario
        .assert_balance(BerylTestEnv::alice(), BerylTestEnv::B20_INITIAL_SUPPLY - TRANSFER_AMOUNT);
    scenario.assert_balance(BerylTestEnv::carol(), 0);

    // Unblock Alice.
    let unblock_alice = scenario.policy_tx(IPolicyRegistry::updateBlocklistCall {
        policyId: blocklist_id,
        blocked: false,
        accounts: vec![BerylTestEnv::alice()],
    });
    let block = scenario.build_block_with_transactions(vec![unblock_alice]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "updateBlocklist(unblock) must succeed");

    // Unblocked sender: transfer must succeed again.
    let unblocked = scenario.env.transfer_b20_tx(
        scenario.token,
        BerylTestEnv::carol(),
        U256::from(TRANSFER_AMOUNT),
    );
    let block = scenario.build_block_with_transactions(vec![unblocked]).await;
    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "transfer from unblocked sender must succeed"
    );
    scenario.assert_balance(
        BerylTestEnv::alice(),
        BerylTestEnv::B20_INITIAL_SUPPLY - TRANSFER_AMOUNT * 2,
    );
    scenario.assert_balance(BerylTestEnv::bob(), TRANSFER_AMOUNT);
    scenario.assert_balance(BerylTestEnv::carol(), TRANSFER_AMOUNT);

    scenario.derive().await;
}

// --- ALWAYS_ALLOW (built-in id = 0) ---

#[tokio::test]
async fn always_allow_policy_never_blocks_b20_transfers() {
    // The TRANSFER_SENDER_POLICY slot defaults to ALWAYS_ALLOW (0) when never written.
    // This test verifies that the zero-initialized default permits all senders.
    let mut scenario = PolicyTransferScenario::new_with_default_policy().await;

    let transfer = scenario.env.transfer_b20_tx(
        scenario.token,
        BerylTestEnv::bob(),
        U256::from(TRANSFER_AMOUNT),
    );
    let block = scenario.build_block_with_transactions(vec![transfer]).await;
    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "transfer must always succeed under ALWAYS_ALLOW policy"
    );
    scenario
        .assert_balance(BerylTestEnv::alice(), BerylTestEnv::B20_INITIAL_SUPPLY - TRANSFER_AMOUNT);
    scenario.assert_balance(BerylTestEnv::bob(), TRANSFER_AMOUNT);

    scenario.derive().await;
}

// --- ALWAYS_BLOCK (built-in id = 1) ---

#[tokio::test]
async fn always_block_policy_always_blocks_b20_transfers() {
    // ALWAYS_BLOCK_ID = 1; set via updatePolicy init call.
    let mut scenario =
        PolicyTransferScenario::new_with_builtin_policy(PolicyRegistryStorage::ALWAYS_BLOCK_ID)
            .await;

    let blocked = scenario.env.transfer_b20_tx(
        scenario.token,
        BerylTestEnv::bob(),
        U256::from(TRANSFER_AMOUNT),
    );
    let block = scenario.build_block_with_transactions(vec![blocked]).await;
    assert!(
        !scenario.env.user_tx_succeeded(&block, 0),
        "transfer must always fail under ALWAYS_BLOCK policy"
    );
    scenario.assert_balance(BerylTestEnv::alice(), BerylTestEnv::B20_INITIAL_SUPPLY);
    scenario.assert_balance(BerylTestEnv::bob(), 0);

    scenario.derive().await;
}

// ---------------------------------------------------------------------------
// Scenario helpers
// ---------------------------------------------------------------------------

/// Test fixture: a funded B-20 token whose `TRANSFER_SENDER_POLICY` is pre-configured.
struct PolicyTransferScenario {
    env: BerylTestEnv,
    token: Address,
    blocks: Vec<(BaseBlock, u64)>,
}

impl PolicyTransferScenario {
    /// Sets up with `TOKEN_FACTORY`, `B20_ASSET`, and `POLICY_REGISTRY` active, creates a custom
    /// `policy_type` policy (Alice as admin), then deploys a B-20 token with the
    /// `TRANSFER_SENDER_POLICY` wired to that policy via an `updatePolicy` init call.
    async fn new_with_custom_policy(
        policy_type: IPolicyRegistry::PolicyType,
        policy_id: u64,
    ) -> Self {
        let env = BerylTestEnv::new();
        let token = env.b20_token_address();
        let mut scenario = Self { env, token, blocks: Vec::new() };

        // Empty block to cross the Beryl activation boundary.
        let beryl_boundary = scenario.env.sequencer.build_empty_block().await;
        scenario.push_block(beryl_boundary);

        // Activate both features in one block.
        let activate_b20 = scenario.env.activate_feature_tx(BerylTestEnv::b20_asset_feature());
        let activate_registry =
            scenario.env.activate_feature_tx(BerylTestEnv::policy_registry_feature());
        let block =
            scenario.build_block_with_transactions(vec![activate_b20, activate_registry]).await;
        assert!(scenario.env.user_tx_succeeded(&block, 0), "B20_ASSET activation must succeed");
        assert!(
            scenario.env.user_tx_succeeded(&block, 1),
            "POLICY_REGISTRY activation must succeed"
        );

        // Create the custom policy with Alice as admin in its own block so that the policy ID
        // exists in committed state when the token's init call checks it.
        let create_policy = scenario.env.create_tx(
            TxKind::Call(PolicyRegistryStorage::ADDRESS),
            Bytes::from(
                IPolicyRegistry::createPolicyCall {
                    admin: BerylTestEnv::alice(),
                    policyType: policy_type,
                }
                .abi_encode(),
            ),
            GAS_LIMIT,
        );
        let block = scenario.build_block_with_transactions(vec![create_policy]).await;
        assert!(scenario.env.user_tx_succeeded(&block, 0), "createPolicy() must succeed");

        // Deploy the B-20 token with the TRANSFER_SENDER_POLICY wired to the custom policy.
        let create_token = scenario.create_token_tx(Some(policy_id));
        let block = scenario.build_block_with_transactions(vec![create_token]).await;
        assert!(
            scenario.env.user_tx_succeeded(&block, 0),
            "B-20 token creation with custom policy must succeed"
        );
        assert!(scenario.env.sequencer.has_code(token), "B-20 token must be deployed");

        scenario
    }

    /// Sets up with all three features active, then deploys a B-20 token with the
    /// `TRANSFER_SENDER_POLICY` set to one of the built-in IDs via an `updatePolicy` init call.
    async fn new_with_builtin_policy(builtin_policy_id: u64) -> Self {
        let env = BerylTestEnv::new();
        let token = env.b20_token_address();
        let mut scenario = Self { env, token, blocks: Vec::new() };

        let beryl_boundary = scenario.env.sequencer.build_empty_block().await;
        scenario.push_block(beryl_boundary);

        let activate_b20 = scenario.env.activate_feature_tx(BerylTestEnv::b20_asset_feature());
        let activate_registry =
            scenario.env.activate_feature_tx(BerylTestEnv::policy_registry_feature());
        let block =
            scenario.build_block_with_transactions(vec![activate_b20, activate_registry]).await;
        assert!(scenario.env.user_tx_succeeded(&block, 0), "B20_ASSET activation must succeed");
        assert!(
            scenario.env.user_tx_succeeded(&block, 1),
            "POLICY_REGISTRY activation must succeed"
        );

        let create_token = scenario.create_token_tx(Some(builtin_policy_id));
        let block = scenario.build_block_with_transactions(vec![create_token]).await;
        assert!(
            scenario.env.user_tx_succeeded(&block, 0),
            "B-20 token creation with built-in policy must succeed"
        );
        assert!(scenario.env.sequencer.has_code(token), "B-20 token must be deployed");

        scenario
    }

    /// Sets up with `TOKEN_FACTORY` and `B20_ASSET` active, then deploys a B-20 token without
    /// an `updatePolicy` init call. The `TRANSFER_SENDER_POLICY` slot defaults to `ALWAYS_ALLOW` (0),
    /// so all transfers are permitted.
    async fn new_with_default_policy() -> Self {
        let env = BerylTestEnv::new();
        let token = env.b20_token_address();
        let mut scenario = Self { env, token, blocks: Vec::new() };

        let beryl_boundary = scenario.env.sequencer.build_empty_block().await;
        scenario.push_block(beryl_boundary);

        let activate_b20 = scenario.env.activate_feature_tx(BerylTestEnv::b20_asset_feature());
        let block = scenario.build_block_with_transactions(vec![activate_b20]).await;
        assert!(scenario.env.user_tx_succeeded(&block, 0), "B20_ASSET activation must succeed");

        // No updatePolicy init call: the TRANSFER_SENDER_POLICY slot reads zero (ALWAYS_ALLOW).
        let create_token = scenario.create_token_tx(None);
        let block = scenario.build_block_with_transactions(vec![create_token]).await;
        assert!(scenario.env.user_tx_succeeded(&block, 0), "B-20 token creation must succeed");
        assert!(scenario.env.sequencer.has_code(token), "B-20 token must be deployed");

        scenario
    }

    /// Builds a `createToken` transaction.
    ///
    /// When `transfer_sender_policy_id` is `Some`, an `updatePolicy` init call wires the
    /// `TRANSFER_SENDER_POLICY` to that ID before minting the initial supply to Alice.
    /// When `None`, only the mint init call is included (default `ALWAYS_ALLOW` semantics).
    fn create_token_tx(&self, transfer_sender_policy_id: Option<u64>) -> BaseTxEnvelope {
        let mut init_calls: Vec<Bytes> = Vec::new();

        if let Some(policy_id) = transfer_sender_policy_id {
            init_calls.push(Bytes::from(
                IB20::updatePolicyCall {
                    policyScope: B20PolicyType::TransferSender.id(),
                    newPolicyId: policy_id,
                }
                .abi_encode(),
            ));
        }

        init_calls.push(Bytes::from(
            IB20::mintCall {
                to: BerylTestEnv::alice(),
                amount: U256::from(BerylTestEnv::B20_INITIAL_SUPPLY),
            }
            .abi_encode(),
        ));

        self.env.create_tx(
            TxKind::Call(B20FactoryStorage::ADDRESS),
            Bytes::from(
                IB20Factory::createB20Call {
                    variant: IB20Factory::B20Variant::ASSET,
                    salt: BerylTestEnv::b20_token_salt(),
                    params: Self::token_params().abi_encode().into(),
                    initCalls: init_calls,
                }
                .abi_encode(),
            ),
            GAS_LIMIT,
        )
    }

    /// Creates a transaction that calls the `PolicyRegistry` precompile, signed by Alice.
    fn policy_tx(&self, call: impl SolCall) -> BaseTxEnvelope {
        self.env.create_tx(
            TxKind::Call(PolicyRegistryStorage::ADDRESS),
            Bytes::from(call.abi_encode()),
            GAS_LIMIT,
        )
    }

    async fn build_block_with_transactions(&mut self, txs: Vec<BaseTxEnvelope>) -> BaseBlock {
        let block = self.env.sequencer.build_next_block_with_transactions(txs).await;
        self.push_block(block.clone());
        block
    }

    fn push_block(&mut self, block: BaseBlock) {
        let block_number = self.blocks.len() as u64 + 1;
        self.blocks.push((block, block_number));
    }

    fn assert_balance(&self, account: Address, expected: u64) {
        assert_eq!(
            self.env.b20_balance(self.token, account),
            U256::from(expected),
            "B-20 balance for {account} must match expected value"
        );
    }

    async fn derive(mut self) {
        let expected_safe_head = self.blocks.len() as u64;
        self.env.derive_blocks(self.blocks, expected_safe_head).await;
    }

    fn token_params() -> IB20Factory::B20AssetCreateParams {
        IB20Factory::B20AssetCreateParams {
            version: B20Variant::Asset.supported_version(),
            name: "Policy B20".to_string(),
            symbol: "PB20".to_string(),
            initialAdmin: BerylTestEnv::alice(),
            decimals: 6,
        }
    }
}
