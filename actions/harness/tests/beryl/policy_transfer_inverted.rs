//! Inverted (NOT) policy-gated B-20 transfer action tests across the Base Denim boundary.
//!
//! Denim resolves the policy registry to V3, which introduces inverted policy IDs: setting the top
//! bit of a policy ID negates that policy's authorization decision. These tests wire a B-20 token's
//! `TRANSFER_SENDER_POLICY` to an *inverted* policy ID and drive real transfers through the EVM to
//! prove the negation end-to-end:
//!   - an inverted blocklist gates transfers like an allowlist (only blocklisted senders may send);
//!   - an inverted allowlist gates transfers like a blocklist (only allowlisted senders may not).
//!
//! Inverted IDs only route on V3, so the token must be wired to them after crossing Denim. The
//! pure-view surface (`invertedPolicyId` round-trip, fail-closed on an unknown base) is covered by
//! the V3 golden and unit tests; the new coverage here is the transfer gating itself.

use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_sol_types::{SolCall, SolValue};
use base_common_consensus::{BaseBlock, BaseTxEnvelope};
use base_common_precompiles::{
    B20FactoryStorage, B20PolicyType, B20Variant, IB20, IB20Factory, IPolicyRegistry,
    PolicyRegistryStorage, PolicyRegistryV3,
};

use crate::env::{BerylTestEnv, DENIM_ACTIVATION_TIMESTAMP};

const GAS_LIMIT: u64 = 10_000_000;

/// Transfer amount used across all inverted-policy gating tests.
const TRANSFER_AMOUNT: u64 = 1_000;

/// Returns the inverted form of `policy_id` (top bit set), matching V3's `invertedPolicyId`.
const fn inverted(policy_id: u64) -> u64 {
    policy_id | PolicyRegistryV3::INVERTED_POLICY_BIT
}

// --- INVERTED BLOCKLIST (gates like an allowlist) ---

#[tokio::test]
async fn inverted_blocklist_gates_b20_transfers_like_an_allowlist() {
    let blocklist_id = BerylTestEnv::policy_id(IPolicyRegistry::PolicyType::BLOCKLIST, 2);
    let mut scenario =
        InvertedPolicyScenario::new(IPolicyRegistry::PolicyType::BLOCKLIST, blocklist_id).await;

    // Empty blocklist: the base authorizes everyone, so the inverted policy authorizes no one.
    // Alice is not blocklisted, so under the inverted policy her transfer must revert.
    let blocked = scenario.env.transfer_b20_tx(
        scenario.token,
        BerylTestEnv::bob(),
        U256::from(TRANSFER_AMOUNT),
    );
    let block = scenario.build_block_with_transactions(vec![blocked]).await;
    assert!(
        !scenario.env.user_tx_succeeded(&block, 0),
        "sender absent from the base blocklist must be unauthorized under the inverted policy"
    );
    scenario.assert_balance(BerylTestEnv::alice(), BerylTestEnv::B20_INITIAL_SUPPLY);
    scenario.assert_balance(BerylTestEnv::bob(), 0);

    // Block Alice in the base policy. The inverted policy now authorizes exactly the blocked set.
    let block_alice = scenario.policy_tx(IPolicyRegistry::updateBlocklistCall {
        policyId: blocklist_id,
        blocked: true,
        accounts: vec![BerylTestEnv::alice()],
    });
    let block = scenario.build_block_with_transactions(vec![block_alice]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "updateBlocklist() must succeed");

    let allowed = scenario.env.transfer_b20_tx(
        scenario.token,
        BerylTestEnv::bob(),
        U256::from(TRANSFER_AMOUNT),
    );
    let block = scenario.build_block_with_transactions(vec![allowed]).await;
    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "blocklisted sender must be authorized under the inverted policy"
    );
    scenario
        .assert_balance(BerylTestEnv::alice(), BerylTestEnv::B20_INITIAL_SUPPLY - TRANSFER_AMOUNT);
    scenario.assert_balance(BerylTestEnv::bob(), TRANSFER_AMOUNT);

    // Unblock Alice: the inverted policy revokes her authorization again.
    let unblock_alice = scenario.policy_tx(IPolicyRegistry::updateBlocklistCall {
        policyId: blocklist_id,
        blocked: false,
        accounts: vec![BerylTestEnv::alice()],
    });
    let block = scenario.build_block_with_transactions(vec![unblock_alice]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "updateBlocklist(remove) must succeed");

    let re_blocked = scenario.env.transfer_b20_tx(
        scenario.token,
        BerylTestEnv::bob(),
        U256::from(TRANSFER_AMOUNT),
    );
    let block = scenario.build_block_with_transactions(vec![re_blocked]).await;
    assert!(
        !scenario.env.user_tx_succeeded(&block, 0),
        "sender removed from the base blocklist must be unauthorized again under the inverted policy"
    );
    scenario
        .assert_balance(BerylTestEnv::alice(), BerylTestEnv::B20_INITIAL_SUPPLY - TRANSFER_AMOUNT);
    scenario.assert_balance(BerylTestEnv::bob(), TRANSFER_AMOUNT);

    scenario.derive().await;
}

// --- INVERTED ALLOWLIST (gates like a blocklist) ---

#[tokio::test]
async fn inverted_allowlist_gates_b20_transfers_like_a_blocklist() {
    let allowlist_id = BerylTestEnv::policy_id(IPolicyRegistry::PolicyType::ALLOWLIST, 2);
    let mut scenario =
        InvertedPolicyScenario::new(IPolicyRegistry::PolicyType::ALLOWLIST, allowlist_id).await;

    // Empty allowlist: the base authorizes no one, so the inverted policy authorizes everyone.
    // Alice is not allowlisted, so under the inverted policy her transfer must succeed.
    let allowed = scenario.env.transfer_b20_tx(
        scenario.token,
        BerylTestEnv::bob(),
        U256::from(TRANSFER_AMOUNT),
    );
    let block = scenario.build_block_with_transactions(vec![allowed]).await;
    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "sender absent from the base allowlist must be authorized under the inverted policy"
    );
    scenario
        .assert_balance(BerylTestEnv::alice(), BerylTestEnv::B20_INITIAL_SUPPLY - TRANSFER_AMOUNT);
    scenario.assert_balance(BerylTestEnv::bob(), TRANSFER_AMOUNT);

    // Add Alice to the base allowlist. The inverted policy now revokes her authorization.
    let add_alice = scenario.policy_tx(IPolicyRegistry::updateAllowlistCall {
        policyId: allowlist_id,
        allowed: true,
        accounts: vec![BerylTestEnv::alice()],
    });
    let block = scenario.build_block_with_transactions(vec![add_alice]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "updateAllowlist() must succeed");

    let blocked = scenario.env.transfer_b20_tx(
        scenario.token,
        BerylTestEnv::bob(),
        U256::from(TRANSFER_AMOUNT),
    );
    let block = scenario.build_block_with_transactions(vec![blocked]).await;
    assert!(
        !scenario.env.user_tx_succeeded(&block, 0),
        "allowlisted sender must be unauthorized under the inverted policy"
    );
    scenario
        .assert_balance(BerylTestEnv::alice(), BerylTestEnv::B20_INITIAL_SUPPLY - TRANSFER_AMOUNT);
    scenario.assert_balance(BerylTestEnv::bob(), TRANSFER_AMOUNT);

    // Remove Alice from the base allowlist: the inverted policy authorizes her again.
    let remove_alice = scenario.policy_tx(IPolicyRegistry::updateAllowlistCall {
        policyId: allowlist_id,
        allowed: false,
        accounts: vec![BerylTestEnv::alice()],
    });
    let block = scenario.build_block_with_transactions(vec![remove_alice]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "updateAllowlist(remove) must succeed");

    let re_allowed = scenario.env.transfer_b20_tx(
        scenario.token,
        BerylTestEnv::bob(),
        U256::from(TRANSFER_AMOUNT),
    );
    let block = scenario.build_block_with_transactions(vec![re_allowed]).await;
    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "sender removed from the base allowlist must be authorized again under the inverted policy"
    );
    scenario.assert_balance(
        BerylTestEnv::alice(),
        BerylTestEnv::B20_INITIAL_SUPPLY - TRANSFER_AMOUNT * 2,
    );
    scenario.assert_balance(BerylTestEnv::bob(), TRANSFER_AMOUNT * 2);

    scenario.derive().await;
}

// ---------------------------------------------------------------------------
// Scenario helpers
// ---------------------------------------------------------------------------

/// Test fixture: a funded B-20 token, deployed after crossing Denim, whose `TRANSFER_SENDER_POLICY`
/// is wired to the *inverted* form of a base policy.
struct InvertedPolicyScenario {
    env: BerylTestEnv,
    token: Address,
    blocks: Vec<(BaseBlock, u64)>,
}

impl InvertedPolicyScenario {
    /// Crosses the Denim boundary (so the registry resolves to V3), activates `B20_ASSET` and
    /// `POLICY_REGISTRY`, creates a `policy_type` base policy with Alice as admin, then deploys a
    /// B-20 token whose `TRANSFER_SENDER_POLICY` is wired to `inverted(policy_id)`.
    async fn new(policy_type: IPolicyRegistry::PolicyType, policy_id: u64) -> Self {
        let env = BerylTestEnv::new_with_denim();
        let token = env.b20_token_address();
        let mut scenario = Self { env, token, blocks: Vec::new() };

        scenario.cross_denim_boundary().await;

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

        // Create the base policy with Alice as admin in its own block so that the policy ID exists
        // in committed state when the token's init call validates it.
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

        // Deploy the B-20 token with TRANSFER_SENDER_POLICY wired to the INVERTED base policy. V3
        // `policy_exists` strips the invert bit and validates the base, so this passes at Denim.
        let create_token = scenario.create_token_tx(inverted(policy_id));
        let block = scenario.build_block_with_transactions(vec![create_token]).await;
        assert!(
            scenario.env.user_tx_succeeded(&block, 0),
            "B-20 token creation with an inverted transfer policy must succeed"
        );
        assert!(scenario.env.sequencer.has_code(token), "B-20 token must be deployed");

        scenario
    }

    /// Builds empty blocks until reaching a Denim-active block with the full (pre-Denim) gas limit.
    ///
    /// The exact fork-activation block is gas-throttled to reserve room for Denim's upgrade system
    /// transactions, so heavy setup transactions would revert there. Crossing with empty blocks
    /// until the gas limit recovers guarantees later setup transactions land in a full-gas block.
    async fn cross_denim_boundary(&mut self) {
        let mut baseline_gas_limit = None;
        loop {
            let block = self.env.sequencer.build_empty_block().await;
            let gas_limit = block.header.gas_limit;
            let timestamp = block.header.timestamp;
            self.push_block(block);

            let baseline = *baseline_gas_limit.get_or_insert(gas_limit);
            if timestamp >= DENIM_ACTIVATION_TIMESTAMP && gas_limit >= baseline {
                break;
            }
        }
    }

    /// Builds a `createB20` transaction that wires `TRANSFER_SENDER_POLICY` to `policy_id` via an
    /// `updatePolicy` init call, then mints the initial supply to Alice.
    fn create_token_tx(&self, policy_id: u64) -> BaseTxEnvelope {
        let init_calls: Vec<Bytes> = vec![
            Bytes::from(
                IB20::updatePolicyCall {
                    policyScope: B20PolicyType::TransferSender.id(),
                    newPolicyId: policy_id,
                }
                .abi_encode(),
            ),
            Bytes::from(
                IB20::mintCall {
                    to: BerylTestEnv::alice(),
                    amount: U256::from(BerylTestEnv::B20_INITIAL_SUPPLY),
                }
                .abi_encode(),
            ),
        ];

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
            name: "Inverted Policy B20".to_string(),
            symbol: "IPB20".to_string(),
            initialAdmin: BerylTestEnv::alice(),
            decimals: 6,
        }
    }
}
