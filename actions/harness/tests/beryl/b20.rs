//! B-20 precompile action tests across the Base Beryl boundary.

use alloy_primitives::{Address, U256};
use base_common_consensus::{BaseBlock, BaseTxEnvelope};

use crate::env::BerylTestEnv;

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
async fn b20_transfer_reverts_while_token_feature_is_deactivated() {
    let mut scenario = B20TokenScenario::new().await;

    let deactivate_b20 = scenario.env.deactivate_feature_tx(BerylTestEnv::b20_token_feature());
    let block = scenario.build_block_with_transactions(vec![deactivate_b20]).await;

    assert!(scenario.env.user_tx_succeeded(&block, 0), "B20_TOKEN deactivation must succeed");

    let transfer_while_deactivated =
        scenario.env.transfer_b20_tx(scenario.token, BerylTestEnv::bob(), U256::from(1));
    let block = scenario.build_block_with_transactions(vec![transfer_while_deactivated]).await;

    assert!(
        !scenario.env.user_tx_succeeded(&block, 0),
        "token transfer must revert when B20_TOKEN is deactivated"
    );
    scenario.assert_balances(BerylTestEnv::B20_INITIAL_SUPPLY, 0, 0);
    scenario.assert_total_supply(BerylTestEnv::B20_INITIAL_SUPPLY);

    let reactivate_b20 = scenario.env.activate_feature_tx(BerylTestEnv::b20_token_feature());
    let block = scenario.build_block_with_transactions(vec![reactivate_b20]).await;

    assert!(scenario.env.user_tx_succeeded(&block, 0), "B20_TOKEN re-activation must succeed");

    let transfer_after_reactivate =
        scenario.env.transfer_b20_tx(scenario.token, BerylTestEnv::bob(), U256::from(1));
    let block = scenario.build_block_with_transactions(vec![transfer_after_reactivate]).await;

    assert!(
        scenario.env.user_tx_succeeded(&block, 0),
        "token transfer must succeed after B20_TOKEN is re-activated"
    );
    scenario.assert_transfer_log(&block, BerylTestEnv::alice(), BerylTestEnv::bob(), 1);
    scenario.assert_balances(BerylTestEnv::B20_INITIAL_SUPPLY - 1, 1, 0);
    scenario.assert_total_supply(BerylTestEnv::B20_INITIAL_SUPPLY);

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

        let activate_factory =
            scenario.env.activate_feature_tx(BerylTestEnv::token_factory_feature());
        let activate_b20 = scenario.env.activate_feature_tx(BerylTestEnv::b20_token_feature());
        let block =
            scenario.build_block_with_transactions(vec![activate_factory, activate_b20]).await;

        assert!(scenario.env.user_tx_succeeded(&block, 0), "TOKEN_FACTORY activation must succeed");
        assert!(scenario.env.user_tx_succeeded(&block, 1), "B20_TOKEN activation must succeed");

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
        assert!(scenario.env.probe_call_succeeded(probe), "B-20 staticcall probe must succeed");
        assert_eq!(
            scenario.env.probe_return_word(probe),
            U256::from(expected),
            "B-20 staticcall probe must return the expected word"
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
