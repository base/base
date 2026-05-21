//! Policy registry precompile action tests across the Base Beryl boundary.

use alloy_consensus::TxReceipt;
use alloy_primitives::{Bytes, TxKind, U256, hex};
use alloy_sol_types::{SolCall, SolEvent};
use base_common_consensus::{BaseBlock, BaseTxEnvelope};
use base_common_precompiles::{IPolicyRegistry, PolicyRegistryStorage};

use crate::env::BerylTestEnv;

const GAS_LIMIT: u64 = 1_000_000;

/// Probe-contract init code.
///
/// Runtime copies calldata, `STATICCALL`s the Beryl policy registry precompile,
/// stores the call success flag in slot 0, and stores the first returned word in slot 1.
const POLICY_REGISTRY_PROBE_INIT_CODE: [u8; 59] = hex!(
    "602f600c600039602f6000f3"
    "3660006000376020600036600073b0300000000000000000000000000000000000005afa8060005560005160015500"
);

const CALL_SUCCESS_SLOT: U256 = U256::ZERO;
const RETURN_WORD_SLOT: U256 = U256::from_limbs([1, 0, 0, 0]);

#[tokio::test]
async fn beryl_enables_policy_registry_singleton_precompile() {
    let mut env = BerylTestEnv::new();
    let probe = env.first_contract_address();
    let policy_exists_call =
        Bytes::from(IPolicyRegistry::policyExistsCall { policyId: 0 }.abi_encode());

    let deploy_probe = env.create_tx(
        TxKind::Create,
        Bytes::from_static(&POLICY_REGISTRY_PROBE_INIT_CODE),
        GAS_LIMIT,
    );
    let pre_beryl_probe = env.create_tx(TxKind::Call(probe), policy_exists_call.clone(), GAS_LIMIT);
    let block1 =
        env.sequencer.build_next_block_with_transactions(vec![deploy_probe, pre_beryl_probe]).await;

    assert!(env.sequencer.has_code(probe), "probe contract must deploy before Beryl");
    assert_ne!(
        env.sequencer.storage_at(probe, RETURN_WORD_SLOT),
        U256::from(1),
        "policy registry must not return true before Beryl"
    );

    // Cross the Beryl activation boundary with an empty block so subsequent blocks execute with
    // the Beryl precompile set.
    let beryl_boundary = env.sequencer.build_empty_block().await;

    // Activate POLICY_REGISTRY in its own block so the state is committed before the probe runs.
    let activate_registry = env.activate_feature_tx(BerylTestEnv::policy_registry_feature());
    let block2 = env.sequencer.build_next_block_with_transactions(vec![activate_registry]).await;

    assert!(env.user_tx_succeeded(&block2, 0), "POLICY_REGISTRY activation must succeed");

    // Block3: probe runs against the committed activated state.
    let post_beryl_probe =
        env.create_tx(TxKind::Call(probe), policy_exists_call.clone(), GAS_LIMIT);
    let block3 = env.sequencer.build_next_block_with_transactions(vec![post_beryl_probe]).await;

    assert_eq!(
        env.sequencer.storage_at(probe, CALL_SUCCESS_SLOT),
        U256::from(1),
        "policy registry staticcall must succeed after activation"
    );
    assert_eq!(
        env.sequencer.storage_at(probe, RETURN_WORD_SLOT),
        U256::from(1),
        "policy registry policyExists(ALWAYS_ALLOW_ID) must return true after activation"
    );

    // -- Deactivation tests --
    // Block4: deactivate POLICY_REGISTRY (committed state before block5).
    let deactivate_registry = env.deactivate_feature_tx(BerylTestEnv::policy_registry_feature());
    let block4 = env.sequencer.build_next_block_with_transactions(vec![deactivate_registry]).await;

    assert!(env.user_tx_succeeded(&block4, 0), "POLICY_REGISTRY deactivation must succeed");

    // Block5: probe's staticcall must fail while POLICY_REGISTRY is deactivated.
    let probe_while_deactivated =
        env.create_tx(TxKind::Call(probe), policy_exists_call.clone(), GAS_LIMIT);
    let block5 =
        env.sequencer.build_next_block_with_transactions(vec![probe_while_deactivated]).await;

    assert_eq!(
        env.sequencer.storage_at(probe, CALL_SUCCESS_SLOT),
        U256::ZERO,
        "policy registry staticcall must fail when POLICY_REGISTRY is deactivated"
    );

    // Block6: re-activate POLICY_REGISTRY (committed state before block7).
    let reactivate_registry = env.activate_feature_tx(BerylTestEnv::policy_registry_feature());
    let block6 = env.sequencer.build_next_block_with_transactions(vec![reactivate_registry]).await;

    assert!(env.user_tx_succeeded(&block6, 0), "POLICY_REGISTRY re-activation must succeed");

    // Block7: probe's staticcall must succeed again after re-activation.
    let probe_after_reactivate = env.create_tx(TxKind::Call(probe), policy_exists_call, GAS_LIMIT);
    let block7 =
        env.sequencer.build_next_block_with_transactions(vec![probe_after_reactivate]).await;

    assert_eq!(
        env.sequencer.storage_at(probe, CALL_SUCCESS_SLOT),
        U256::from(1),
        "policy registry staticcall must succeed after re-activation"
    );
    assert_eq!(
        env.sequencer.storage_at(probe, RETURN_WORD_SLOT),
        U256::from(1),
        "policy registry policyExists(ALWAYS_ALLOW_ID) must return true after re-activation"
    );

    env.derive_blocks(
        [
            (block1, 1),
            (beryl_boundary, 2),
            (block2, 3),
            (block3, 4),
            (block4, 5),
            (block5, 6),
            (block6, 7),
            (block7, 8),
        ],
        8,
    )
    .await;
}

#[tokio::test]
async fn policy_registry_action_tests_cover_policy_lifecycle_and_views() {
    let mut scenario = PolicyRegistryScenario::new().await;
    let allowlist_id = policy_id(IPolicyRegistry::PolicyType::ALLOWLIST, 2);
    let blocklist_id = policy_id(IPolicyRegistry::PolicyType::BLOCKLIST, 3);

    let create_allowlist = scenario.tx(IPolicyRegistry::createPolicyCall {
        admin: BerylTestEnv::alice(),
        policyType: IPolicyRegistry::PolicyType::ALLOWLIST,
    });
    let block = scenario.build_block_with_transactions(vec![create_allowlist]).await;

    assert!(scenario.env.user_tx_succeeded(&block, 0), "createPolicy() must succeed");
    scenario.assert_policy_log(
        &block,
        0,
        IPolicyRegistry::PolicyCreated {
            policyId: allowlist_id,
            creator: BerylTestEnv::alice(),
            policyType: IPolicyRegistry::PolicyType::ALLOWLIST,
        }
        .encode_log_data(),
    );
    scenario.assert_policy_log(
        &block,
        0,
        IPolicyRegistry::PolicyAdminUpdated {
            policyId: allowlist_id,
            previousAdmin: alloy_primitives::Address::ZERO,
            newAdmin: BerylTestEnv::alice(),
        }
        .encode_log_data(),
    );

    scenario
        .assert_probe_word(
            "policyExists(allowlist)",
            IPolicyRegistry::policyExistsCall { policyId: allowlist_id }.abi_encode(),
            U256::ONE,
        )
        .await;
    scenario
        .assert_probe_word(
            "policyType(allowlist)",
            IPolicyRegistry::policyTypeCall { policyId: allowlist_id }.abi_encode(),
            U256::from(IPolicyRegistry::PolicyType::ALLOWLIST as u8),
        )
        .await;
    scenario
        .assert_probe_word(
            "policyAdmin(allowlist)",
            IPolicyRegistry::policyAdminCall { policyId: allowlist_id }.abi_encode(),
            word_from_address(BerylTestEnv::alice()),
        )
        .await;
    scenario
        .assert_probe_word(
            "pendingPolicyAdmin(allowlist)",
            IPolicyRegistry::pendingPolicyAdminCall { policyId: allowlist_id }.abi_encode(),
            U256::ZERO,
        )
        .await;

    let update_allowlist = scenario.tx(IPolicyRegistry::updateAllowlistCall {
        policyId: allowlist_id,
        allowed: true,
        accounts: vec![BerylTestEnv::bob()],
    });
    let block = scenario.build_block_with_transactions(vec![update_allowlist]).await;

    assert!(scenario.env.user_tx_succeeded(&block, 0), "updateAllowlist() must succeed");
    scenario.assert_policy_log(
        &block,
        0,
        IPolicyRegistry::AllowlistUpdated {
            policyId: allowlist_id,
            updater: BerylTestEnv::alice(),
            allowed: true,
            accounts: vec![BerylTestEnv::bob()],
        }
        .encode_log_data(),
    );
    scenario
        .assert_probe_word(
            "isAuthorized(allowlist member)",
            IPolicyRegistry::isAuthorizedCall {
                policyId: allowlist_id,
                account: BerylTestEnv::bob(),
            }
            .abi_encode(),
            U256::ONE,
        )
        .await;
    scenario
        .assert_probe_word(
            "isAuthorized(allowlist non-member)",
            IPolicyRegistry::isAuthorizedCall {
                policyId: allowlist_id,
                account: BerylTestEnv::carol(),
            }
            .abi_encode(),
            U256::ZERO,
        )
        .await;

    let stage_admin = scenario.tx(IPolicyRegistry::stageUpdateAdminCall {
        policyId: allowlist_id,
        newAdmin: BerylTestEnv::bob(),
    });
    let block = scenario.build_block_with_transactions(vec![stage_admin]).await;

    assert!(scenario.env.user_tx_succeeded(&block, 0), "stageUpdateAdmin() must succeed");
    scenario.assert_policy_log(
        &block,
        0,
        IPolicyRegistry::PolicyAdminStaged {
            policyId: allowlist_id,
            previousAdmin: BerylTestEnv::alice(),
            newAdmin: BerylTestEnv::bob(),
        }
        .encode_log_data(),
    );
    scenario
        .assert_probe_word(
            "pendingPolicyAdmin(staged)",
            IPolicyRegistry::pendingPolicyAdminCall { policyId: allowlist_id }.abi_encode(),
            word_from_address(BerylTestEnv::bob()),
        )
        .await;

    let finalize_admin =
        scenario.bob_tx(IPolicyRegistry::finalizeUpdateAdminCall { policyId: allowlist_id });
    let block = scenario.build_block_with_transactions(vec![finalize_admin]).await;

    assert!(scenario.env.user_tx_succeeded(&block, 0), "finalizeUpdateAdmin() must succeed");
    scenario.assert_policy_log(
        &block,
        0,
        IPolicyRegistry::PolicyAdminUpdated {
            policyId: allowlist_id,
            previousAdmin: BerylTestEnv::alice(),
            newAdmin: BerylTestEnv::bob(),
        }
        .encode_log_data(),
    );
    scenario
        .assert_probe_word(
            "policyAdmin(after finalize)",
            IPolicyRegistry::policyAdminCall { policyId: allowlist_id }.abi_encode(),
            word_from_address(BerylTestEnv::bob()),
        )
        .await;

    let create_blocklist = scenario.tx(IPolicyRegistry::createPolicyWithAccountsCall {
        admin: BerylTestEnv::bob(),
        policyType: IPolicyRegistry::PolicyType::BLOCKLIST,
        accounts: vec![BerylTestEnv::bob()],
    });
    let block = scenario.build_block_with_transactions(vec![create_blocklist]).await;

    assert!(scenario.env.user_tx_succeeded(&block, 0), "createPolicyWithAccounts() must succeed");
    scenario.assert_policy_log(
        &block,
        0,
        IPolicyRegistry::PolicyCreated {
            policyId: blocklist_id,
            creator: BerylTestEnv::alice(),
            policyType: IPolicyRegistry::PolicyType::BLOCKLIST,
        }
        .encode_log_data(),
    );
    scenario.assert_policy_log(
        &block,
        0,
        IPolicyRegistry::BlocklistUpdated {
            policyId: blocklist_id,
            updater: BerylTestEnv::alice(),
            blocked: true,
            accounts: vec![BerylTestEnv::bob()],
        }
        .encode_log_data(),
    );
    scenario
        .assert_probe_word(
            "isAuthorized(blocked member)",
            IPolicyRegistry::isAuthorizedCall {
                policyId: blocklist_id,
                account: BerylTestEnv::bob(),
            }
            .abi_encode(),
            U256::ZERO,
        )
        .await;

    let update_blocklist = scenario.bob_tx(IPolicyRegistry::updateBlocklistCall {
        policyId: blocklist_id,
        blocked: false,
        accounts: vec![BerylTestEnv::bob()],
    });
    let block = scenario.build_block_with_transactions(vec![update_blocklist]).await;

    assert!(scenario.env.user_tx_succeeded(&block, 0), "updateBlocklist() must succeed");
    scenario.assert_policy_log(
        &block,
        0,
        IPolicyRegistry::BlocklistUpdated {
            policyId: blocklist_id,
            updater: BerylTestEnv::bob(),
            blocked: false,
            accounts: vec![BerylTestEnv::bob()],
        }
        .encode_log_data(),
    );
    scenario
        .assert_probe_word(
            "isAuthorized(unblocked member)",
            IPolicyRegistry::isAuthorizedCall {
                policyId: blocklist_id,
                account: BerylTestEnv::bob(),
            }
            .abi_encode(),
            U256::ONE,
        )
        .await;

    let renounce_admin =
        scenario.bob_tx(IPolicyRegistry::renounceAdminCall { policyId: allowlist_id });
    let block = scenario.build_block_with_transactions(vec![renounce_admin]).await;

    assert!(scenario.env.user_tx_succeeded(&block, 0), "renounceAdmin() must succeed");
    scenario.assert_policy_log(
        &block,
        0,
        IPolicyRegistry::PolicyAdminUpdated {
            policyId: allowlist_id,
            previousAdmin: BerylTestEnv::bob(),
            newAdmin: alloy_primitives::Address::ZERO,
        }
        .encode_log_data(),
    );
    scenario
        .assert_probe_word(
            "policyAdmin(after renounce)",
            IPolicyRegistry::policyAdminCall { policyId: allowlist_id }.abi_encode(),
            U256::ZERO,
        )
        .await;

    scenario.derive().await;
}

struct PolicyRegistryScenario {
    env: BerylTestEnv,
    probe: alloy_primitives::Address,
    blocks: Vec<(BaseBlock, u64)>,
}

impl PolicyRegistryScenario {
    async fn new() -> Self {
        let env = BerylTestEnv::new();
        let mut scenario = Self { env, probe: alloy_primitives::Address::ZERO, blocks: Vec::new() };

        scenario.build_empty_block().await;

        let activate = scenario.env.activate_feature_tx(BerylTestEnv::policy_registry_feature());
        let block = scenario.build_block_with_transactions(vec![activate]).await;
        assert!(
            scenario.env.user_tx_succeeded(&block, 0),
            "POLICY_REGISTRY activation must succeed"
        );

        let (probe, deploy_probe) =
            scenario.env.deploy_staticcall_probe_tx(PolicyRegistryStorage::ADDRESS);
        scenario.probe = probe;
        let block = scenario.build_block_with_transactions(vec![deploy_probe]).await;
        assert!(scenario.env.user_tx_succeeded(&block, 0), "policy probe deployment must succeed");

        scenario
    }

    async fn build_empty_block(&mut self) {
        let block = self.env.sequencer.build_empty_block().await;
        self.push_block(block);
    }

    async fn build_block_with_transactions(&mut self, txs: Vec<BaseTxEnvelope>) -> BaseBlock {
        let block = self.env.sequencer.build_next_block_with_transactions(txs).await;
        self.push_block(block.clone());
        block
    }

    fn tx(&self, call: impl SolCall) -> BaseTxEnvelope {
        self.env.create_tx(
            TxKind::Call(PolicyRegistryStorage::ADDRESS),
            Bytes::from(call.abi_encode()),
            GAS_LIMIT,
        )
    }

    fn bob_tx(&mut self, call: impl SolCall) -> BaseTxEnvelope {
        self.env.create_bob_tx(
            TxKind::Call(PolicyRegistryStorage::ADDRESS),
            Bytes::from(call.abi_encode()),
            GAS_LIMIT,
        )
    }

    async fn assert_probe_word(&mut self, label: &'static str, calldata: Vec<u8>, expected: U256) {
        let tx = self.env.call_staticcall_probe_tx(
            self.probe,
            Bytes::from(calldata),
            BerylTestEnv::B20_PROBE_GAS_LIMIT,
        );
        let block = self.build_block_with_transactions(vec![tx]).await;
        assert!(self.env.user_tx_succeeded(&block, 0), "{label} probe tx must succeed");
        assert!(self.env.probe_call_succeeded(self.probe), "{label} staticcall must succeed");
        assert_eq!(
            self.env.probe_return_word(self.probe),
            expected,
            "{label} staticcall must return the expected word"
        );
    }

    #[track_caller]
    fn assert_policy_log(
        &self,
        block: &BaseBlock,
        user_tx_index: usize,
        expected: alloy_primitives::LogData,
    ) {
        let receipt = self.env.user_tx_receipt(block, user_tx_index);
        assert!(
            receipt
                .logs()
                .iter()
                .any(|log| log.address == PolicyRegistryStorage::ADDRESS && log.data == expected),
            "policy-registry transaction {user_tx_index} must emit the expected event; expected={expected:?}, logs={:?}",
            receipt.logs()
        );
    }

    async fn derive(mut self) {
        let expected_safe_head = self.blocks.len() as u64;
        self.env.derive_blocks(self.blocks, expected_safe_head).await;
    }

    fn push_block(&mut self, block: BaseBlock) {
        let block_number = self.blocks.len() as u64 + 1;
        self.blocks.push((block, block_number));
    }
}

const fn policy_id(policy_type: IPolicyRegistry::PolicyType, counter: u64) -> u64 {
    (policy_type as u64) << 56 | counter
}

fn word_from_address(address: alloy_primitives::Address) -> U256 {
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(address.as_slice());
    U256::from_be_slice(&word)
}
