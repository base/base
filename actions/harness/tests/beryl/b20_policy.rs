//! Action tests proving that B20 token transfers are gated by the policy registry.
//!
//! Each test activates `TOKEN_FACTORY`, `B20_ASSET`, and `POLICY_REGISTRY` together,
//! creates a token, wires a policy via `updatePolicy`, and asserts that transfers
//! revert or succeed based on policy membership.

use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_sol_types::SolCall;
use base_common_consensus::{BaseBlock, BaseTxEnvelope};
use base_common_precompiles::{B20PolicyType, IB20, IPolicyRegistry, PolicyRegistryStorage};

use crate::env::BerylTestEnv;

/// Transfer amount used in setup (seeding bob with tokens).
const SEED_AMOUNT: U256 = U256::from_limbs([100_000, 0, 0, 0]);

/// Amount transferred in each policy-gated transfer assertion.
const TRANSFER_AMOUNT: U256 = U256::from_limbs([1, 0, 0, 0]);

/// ALLOWLIST policy wired to `TransferSender` blocks non-members from sending.
///
/// 1. Alice seeds bob with tokens (default `ALWAYS_ALLOW` policy, succeeds).
/// 2. Create ALLOWLIST policy; wire it to `TransferSender`.
/// 3. Bob tries to transfer; reverts (not in allowlist).
/// 4. Admin adds bob to the allowlist.
/// 5. Bob transfers again; succeeds.
#[tokio::test]
async fn b20_allowlist_sender_policy_blocks_non_members() {
    let mut scenario = B20PolicyScenario::new().await;

    let seed_bob =
        scenario.token_tx(IB20::transferCall { to: BerylTestEnv::bob(), amount: SEED_AMOUNT });
    let block = scenario.build_block(vec![seed_bob]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "seeding bob must succeed");
    scenario.assert_balance(BerylTestEnv::alice(), BerylTestEnv::B20_INITIAL_SUPPLY - 100_000);
    scenario.assert_balance(BerylTestEnv::bob(), 100_000);

    let allowlist_id = BerylTestEnv::policy_id(IPolicyRegistry::PolicyType::ALLOWLIST, 2);
    let create_policy = scenario.policy_tx(IPolicyRegistry::createPolicyCall {
        admin: BerylTestEnv::alice(),
        policyType: IPolicyRegistry::PolicyType::ALLOWLIST,
    });
    let block = scenario.build_block(vec![create_policy]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "createPolicy(ALLOWLIST) must succeed");

    let wire = scenario.token_tx(IB20::updatePolicyCall {
        policyScope: B20PolicyType::TransferSender.id(),
        newPolicyId: allowlist_id,
    });
    let block = scenario.build_block(vec![wire]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "updatePolicy must succeed");

    let blocked = scenario
        .bob_token_tx(IB20::transferCall { to: BerylTestEnv::carol(), amount: TRANSFER_AMOUNT });
    let block = scenario.build_block(vec![blocked]).await;
    assert!(
        !scenario.env.user_tx_succeeded(&block, 0),
        "transfer from non-member must revert when ALLOWLIST sender policy is set"
    );
    scenario.assert_balance(BerylTestEnv::bob(), 100_000);
    scenario.assert_balance(BerylTestEnv::carol(), 0);

    let add_bob = scenario.policy_tx(IPolicyRegistry::updateAllowlistCall {
        policyId: allowlist_id,
        allowed: true,
        accounts: vec![BerylTestEnv::bob()],
    });
    let block = scenario.build_block(vec![add_bob]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "updateAllowlist must succeed");

    let allowed = scenario
        .bob_token_tx(IB20::transferCall { to: BerylTestEnv::carol(), amount: TRANSFER_AMOUNT });
    let block = scenario.build_block(vec![allowed]).await;
    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "transfer from allowlisted member must succeed"
    );
    scenario.assert_balance(BerylTestEnv::bob(), 99_999);
    scenario.assert_balance(BerylTestEnv::carol(), 1);

    scenario.derive().await;
}

/// BLOCKLIST policy wired to `TransferSender` blocks listed accounts from sending.
///
/// 1. Alice seeds bob with tokens (default `ALWAYS_ALLOW` policy, succeeds).
/// 2. Create BLOCKLIST policy; wire it to `TransferSender`.
/// 3. Bob transfers; succeeds (not in blocklist).
/// 4. Admin adds bob to the blocklist.
/// 5. Bob tries to transfer; reverts.
#[tokio::test]
async fn b20_blocklist_sender_policy_blocks_listed_accounts() {
    let mut scenario = B20PolicyScenario::new().await;

    let seed_bob =
        scenario.token_tx(IB20::transferCall { to: BerylTestEnv::bob(), amount: SEED_AMOUNT });
    let block = scenario.build_block(vec![seed_bob]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "seeding bob must succeed");
    scenario.assert_balance(BerylTestEnv::alice(), BerylTestEnv::B20_INITIAL_SUPPLY - 100_000);
    scenario.assert_balance(BerylTestEnv::bob(), 100_000);

    let blocklist_id = BerylTestEnv::policy_id(IPolicyRegistry::PolicyType::BLOCKLIST, 2);
    let create_policy = scenario.policy_tx(IPolicyRegistry::createPolicyCall {
        admin: BerylTestEnv::alice(),
        policyType: IPolicyRegistry::PolicyType::BLOCKLIST,
    });
    let block = scenario.build_block(vec![create_policy]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "createPolicy(BLOCKLIST) must succeed");

    let wire = scenario.token_tx(IB20::updatePolicyCall {
        policyScope: B20PolicyType::TransferSender.id(),
        newPolicyId: blocklist_id,
    });
    let block = scenario.build_block(vec![wire]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "updatePolicy must succeed");

    let first_transfer = scenario
        .bob_token_tx(IB20::transferCall { to: BerylTestEnv::carol(), amount: TRANSFER_AMOUNT });
    let block = scenario.build_block(vec![first_transfer]).await;
    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "transfer from non-blocked account must succeed"
    );
    scenario.assert_balance(BerylTestEnv::bob(), 99_999);
    scenario.assert_balance(BerylTestEnv::carol(), 1);

    let block_bob = scenario.policy_tx(IPolicyRegistry::updateBlocklistCall {
        policyId: blocklist_id,
        blocked: true,
        accounts: vec![BerylTestEnv::bob()],
    });
    let block = scenario.build_block(vec![block_bob]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "updateBlocklist must succeed");

    let blocked = scenario
        .bob_token_tx(IB20::transferCall { to: BerylTestEnv::carol(), amount: TRANSFER_AMOUNT });
    let block = scenario.build_block(vec![blocked]).await;
    assert!(
        !scenario.env.user_tx_succeeded(&block, 0),
        "transfer from blocked account must revert"
    );
    scenario.assert_balance(BerylTestEnv::bob(), 99_999);
    scenario.assert_balance(BerylTestEnv::carol(), 1);

    scenario.derive().await;
}

/// Wiring the built-in `ALWAYS_BLOCK` policy to `TransferSender` blocks every sender immediately.
///
/// No allowlist entries are needed: `ALWAYS_BLOCK_ID` denies all accounts unconditionally.
#[tokio::test]
async fn b20_always_block_sender_policy_blocks_all_transfers() {
    let mut scenario = B20PolicyScenario::new().await;

    let wire = scenario.token_tx(IB20::updatePolicyCall {
        policyScope: B20PolicyType::TransferSender.id(),
        newPolicyId: PolicyRegistryStorage::ALWAYS_BLOCK_ID,
    });
    let block = scenario.build_block(vec![wire]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "updatePolicy must succeed");

    let blocked =
        scenario.token_tx(IB20::transferCall { to: BerylTestEnv::bob(), amount: TRANSFER_AMOUNT });
    let block = scenario.build_block(vec![blocked]).await;
    assert!(
        !scenario.env.user_tx_succeeded(&block, 0),
        "transfer must revert when ALWAYS_BLOCK sender policy is set"
    );
    scenario.assert_balance(BerylTestEnv::alice(), BerylTestEnv::B20_INITIAL_SUPPLY);
    scenario.assert_balance(BerylTestEnv::bob(), 0);

    scenario.derive().await;
}

/// ALLOWLIST policy wired to `TransferReceiver` blocks non-members from receiving.
#[tokio::test]
async fn b20_allowlist_receiver_policy_blocks_non_members() {
    let mut scenario = B20PolicyScenario::new().await;

    let allowlist_id = BerylTestEnv::policy_id(IPolicyRegistry::PolicyType::ALLOWLIST, 2);
    let create_policy = scenario.policy_tx(IPolicyRegistry::createPolicyCall {
        admin: BerylTestEnv::alice(),
        policyType: IPolicyRegistry::PolicyType::ALLOWLIST,
    });
    let block = scenario.build_block(vec![create_policy]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "createPolicy(ALLOWLIST) must succeed");

    let wire = scenario.token_tx(IB20::updatePolicyCall {
        policyScope: B20PolicyType::TransferReceiver.id(),
        newPolicyId: allowlist_id,
    });
    let block = scenario.build_block(vec![wire]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "updatePolicy must succeed");

    let blocked =
        scenario.token_tx(IB20::transferCall { to: BerylTestEnv::bob(), amount: TRANSFER_AMOUNT });
    let block = scenario.build_block(vec![blocked]).await;
    assert!(
        !scenario.env.user_tx_succeeded(&block, 0),
        "transfer to non-member must revert when ALLOWLIST receiver policy is set"
    );
    scenario.assert_balance(BerylTestEnv::alice(), BerylTestEnv::B20_INITIAL_SUPPLY);
    scenario.assert_balance(BerylTestEnv::bob(), 0);

    let add_bob = scenario.policy_tx(IPolicyRegistry::updateAllowlistCall {
        policyId: allowlist_id,
        allowed: true,
        accounts: vec![BerylTestEnv::bob()],
    });
    let block = scenario.build_block(vec![add_bob]).await;
    assert!(scenario.env.user_tx_succeeded(&block, 0), "updateAllowlist must succeed");

    let allowed =
        scenario.token_tx(IB20::transferCall { to: BerylTestEnv::bob(), amount: TRANSFER_AMOUNT });
    let block = scenario.build_block(vec![allowed]).await;
    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "transfer to allowlisted receiver must succeed"
    );
    scenario.assert_balance(BerylTestEnv::alice(), BerylTestEnv::B20_INITIAL_SUPPLY - 1);
    scenario.assert_balance(BerylTestEnv::bob(), 1);

    scenario.derive().await;
}

struct B20PolicyScenario {
    env: BerylTestEnv,
    token: Address,
    blocks: Vec<(BaseBlock, u64)>,
}

impl B20PolicyScenario {
    async fn new() -> Self {
        let env = BerylTestEnv::new();
        let token = env.b20_token_address();
        let mut scenario = Self { env, token, blocks: Vec::new() };

        scenario.build_block(vec![]).await;

        let act_b20 = scenario.env.activate_feature_tx(BerylTestEnv::b20_asset_feature());
        let act_policy = scenario.env.activate_feature_tx(BerylTestEnv::policy_registry_feature());
        let block = scenario.build_block(vec![act_b20, act_policy]).await;
        assert!(scenario.env.user_tx_succeeded(&block, 0), "B20_ASSET activation must succeed");
        assert!(
            scenario.env.user_tx_succeeded(&block, 1),
            "POLICY_REGISTRY activation must succeed"
        );

        let create = scenario.env.create_b20_token_tx();
        let block = scenario.build_block(vec![create]).await;
        assert!(scenario.env.user_tx_succeeded(&block, 0), "B20 token creation must succeed");

        scenario
    }

    async fn build_block(&mut self, txs: Vec<BaseTxEnvelope>) -> BaseBlock {
        let block = self.env.sequencer.build_next_block_with_transactions(txs).await;
        let block_number = self.blocks.len() as u64 + 1;
        self.blocks.push((block.clone(), block_number));
        block
    }

    fn token_tx(&self, call: impl SolCall) -> BaseTxEnvelope {
        self.env.create_tx(
            TxKind::Call(self.token),
            Bytes::from(call.abi_encode()),
            BerylTestEnv::B20_GAS_LIMIT,
        )
    }

    fn bob_token_tx(&mut self, call: impl SolCall) -> BaseTxEnvelope {
        self.env.create_bob_tx(
            TxKind::Call(self.token),
            Bytes::from(call.abi_encode()),
            BerylTestEnv::B20_GAS_LIMIT,
        )
    }

    fn policy_tx(&self, call: impl SolCall) -> BaseTxEnvelope {
        self.env.create_tx(
            TxKind::Call(PolicyRegistryStorage::ADDRESS),
            Bytes::from(call.abi_encode()),
            BerylTestEnv::B20_GAS_LIMIT,
        )
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
}
