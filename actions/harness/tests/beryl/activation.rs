//! Activation registry precompile action tests across the Base Beryl boundary.

use alloy_consensus::TxReceipt;
use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_sol_types::{SolCall, SolEvent};
use base_common_precompiles::{ActivationFeature, ActivationRegistryStorage, IActivationRegistry};

use crate::env::BerylTestEnv;

const GAS_LIMIT: u64 = 1_000_000;
const FEATURE: alloy_primitives::B256 = ActivationFeature::B20Asset.id();

#[tokio::test]
async fn beryl_enables_activation_registry_admin_and_feature_lifecycle() {
    let mut env = BerylTestEnv::new();
    let (probe, deploy_probe) = env.deploy_staticcall_probe_tx(ActivationRegistryStorage::ADDRESS);

    let admin_call = Bytes::from(IActivationRegistry::adminCall {}.abi_encode());
    let pre_beryl_admin =
        env.call_staticcall_probe_tx(probe, admin_call.clone(), BerylTestEnv::B20_PROBE_GAS_LIMIT);
    let block1 =
        env.sequencer.build_next_block_with_transactions(vec![deploy_probe, pre_beryl_admin]).await;

    assert!(env.user_tx_succeeded(&block1, 0), "activation-registry probe must deploy");
    assert_ne!(
        env.probe_return_word(probe),
        word_from_address(BerylTestEnv::alice()),
        "activation registry admin must not be returned before Beryl"
    );

    let beryl_boundary = env.sequencer.build_empty_block().await;

    let post_beryl_admin =
        env.call_staticcall_probe_tx(probe, admin_call, BerylTestEnv::B20_PROBE_GAS_LIMIT);
    let block2 = env.sequencer.build_next_block_with_transactions(vec![post_beryl_admin]).await;

    assert!(env.probe_call_succeeded(probe), "admin() staticcall must succeed after Beryl");
    assert_eq!(
        env.probe_return_word(probe),
        word_from_address(BerylTestEnv::alice()),
        "admin() must return the harness activation admin"
    );

    let is_activated_call =
        Bytes::from(IActivationRegistry::isActivatedCall { feature: FEATURE }.abi_encode());
    let inactive_probe = env.call_staticcall_probe_tx(
        probe,
        is_activated_call.clone(),
        BerylTestEnv::B20_PROBE_GAS_LIMIT,
    );
    let block3 = env.sequencer.build_next_block_with_transactions(vec![inactive_probe]).await;

    assert!(env.probe_call_succeeded(probe), "isActivated() staticcall must succeed");
    assert_eq!(env.probe_return_word(probe), U256::ZERO, "feature must start inactive");

    let activate = env.activate_feature_tx(FEATURE);
    let block4 = env.sequencer.build_next_block_with_transactions(vec![activate]).await;

    assert!(env.user_tx_succeeded(&block4, 0), "admin activate(feature) must succeed");
    assert_activation_log(&env, &block4, true);

    let activate_again = env.activate_feature_tx(FEATURE);
    let block5 = env.sequencer.build_next_block_with_transactions(vec![activate_again]).await;

    assert!(!env.user_tx_succeeded(&block5, 0), "repeated activate(feature) must revert");

    let active_probe = env.call_staticcall_probe_tx(
        probe,
        is_activated_call.clone(),
        BerylTestEnv::B20_PROBE_GAS_LIMIT,
    );
    let block6 = env.sequencer.build_next_block_with_transactions(vec![active_probe]).await;

    assert!(env.probe_call_succeeded(probe), "isActivated() staticcall must succeed");
    assert_eq!(env.probe_return_word(probe), U256::ONE, "feature must be active");

    let deactivate = env.deactivate_feature_tx(FEATURE);
    let block7 = env.sequencer.build_next_block_with_transactions(vec![deactivate]).await;

    assert!(env.user_tx_succeeded(&block7, 0), "admin deactivate(feature) must succeed");
    assert_activation_log(&env, &block7, false);

    let deactivate_again = env.deactivate_feature_tx(FEATURE);
    let block8 = env.sequencer.build_next_block_with_transactions(vec![deactivate_again]).await;

    assert!(!env.user_tx_succeeded(&block8, 0), "repeated deactivate(feature) must revert");

    let unauthorized = env.create_bob_tx(
        TxKind::Call(ActivationRegistryStorage::ADDRESS),
        Bytes::from(IActivationRegistry::activateCall { feature: FEATURE }.abi_encode()),
        GAS_LIMIT,
    );
    let block9 = env.sequencer.build_next_block_with_transactions(vec![unauthorized]).await;

    assert!(!env.user_tx_succeeded(&block9, 0), "non-admin activate(feature) must revert");

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
            (block8, 9),
            (block9, 10),
        ],
        10,
    )
    .await;
}

#[tokio::test]
async fn cobalt_enables_state_backed_activation_admin_rotation() {
    let mut env = BerylTestEnv::new_with_cobalt();
    let (probe, deploy_probe) = env.deploy_staticcall_probe_tx(ActivationRegistryStorage::ADDRESS);

    let pre_beryl = env.sequencer.build_next_block_with_transactions(vec![deploy_probe]).await;
    assert!(env.user_tx_succeeded(&pre_beryl, 0), "activation-registry probe must deploy");

    let beryl_boundary = env.sequencer.build_empty_block().await;

    let pre_cobalt_set_admin = env.set_activation_admin_tx(BerylTestEnv::bob());
    let pre_cobalt =
        env.sequencer.build_next_block_with_transactions(vec![pre_cobalt_set_admin]).await;
    assert!(
        !env.user_tx_succeeded(&pre_cobalt, 0),
        "setAdmin(newAdmin) must revert after Beryl but before Cobalt"
    );

    let cobalt_boundary = env.sequencer.build_empty_block().await;

    let set_admin = env.set_activation_admin_tx(BerylTestEnv::bob());
    let set_admin_block = env.sequencer.build_next_block_with_transactions(vec![set_admin]).await;
    assert!(
        env.user_tx_succeeded(&set_admin_block, 0),
        "setAdmin(newAdmin) must succeed at Cobalt"
    );
    assert_admin_changed_log(&env, &set_admin_block);

    let admin_call = Bytes::from(IActivationRegistry::adminCall {}.abi_encode());
    let admin_probe =
        env.call_staticcall_probe_tx(probe, admin_call, BerylTestEnv::B20_PROBE_GAS_LIMIT);
    let admin_probe_block =
        env.sequencer.build_next_block_with_transactions(vec![admin_probe]).await;
    assert!(env.probe_call_succeeded(probe), "admin() staticcall must succeed after rotation");
    assert_eq!(
        env.probe_return_word(probe),
        word_from_address(BerylTestEnv::bob()),
        "admin() must return the state-backed activation admin"
    );

    let old_admin_activate = env.activate_feature_tx(FEATURE);
    let old_admin_block =
        env.sequencer.build_next_block_with_transactions(vec![old_admin_activate]).await;
    assert!(
        !env.user_tx_succeeded(&old_admin_block, 0),
        "previous admin must not activate features after rotation"
    );

    let new_admin_activate = env.create_bob_tx(
        TxKind::Call(ActivationRegistryStorage::ADDRESS),
        Bytes::from(IActivationRegistry::activateCall { feature: FEATURE }.abi_encode()),
        GAS_LIMIT,
    );
    let new_admin_block =
        env.sequencer.build_next_block_with_transactions(vec![new_admin_activate]).await;
    assert!(env.user_tx_succeeded(&new_admin_block, 0), "new admin must activate features");
    assert_activation_log_from(&env, &new_admin_block, true, BerylTestEnv::bob());

    env.derive_blocks(
        [
            (pre_beryl, 1),
            (beryl_boundary, 2),
            (pre_cobalt, 3),
            (cobalt_boundary, 4),
            (set_admin_block, 5),
            (admin_probe_block, 6),
            (old_admin_block, 7),
            (new_admin_block, 8),
        ],
        8,
    )
    .await;
}

fn assert_activation_log(
    env: &BerylTestEnv,
    block: &base_common_consensus::BaseBlock,
    active: bool,
) {
    assert_activation_log_from(env, block, active, BerylTestEnv::alice());
}

fn assert_activation_log_from(
    env: &BerylTestEnv,
    block: &base_common_consensus::BaseBlock,
    active: bool,
    caller: Address,
) {
    let expected = if active {
        IActivationRegistry::FeatureActivated { feature: FEATURE, caller }.encode_log_data()
    } else {
        IActivationRegistry::FeatureDeactivated { feature: FEATURE, caller }.encode_log_data()
    };
    assert!(
        env.user_tx_receipt(block, 0)
            .logs()
            .iter()
            .any(|log| log.address == ActivationRegistryStorage::ADDRESS && log.data == expected),
        "activation transition must emit the expected event"
    );
}

fn assert_admin_changed_log(env: &BerylTestEnv, block: &base_common_consensus::BaseBlock) {
    let expected = IActivationRegistry::AdminChanged {
        previousAdmin: BerylTestEnv::alice(),
        newAdmin: BerylTestEnv::bob(),
        caller: BerylTestEnv::alice(),
    }
    .encode_log_data();
    assert!(
        env.user_tx_receipt(block, 0)
            .logs()
            .iter()
            .any(|log| log.address == ActivationRegistryStorage::ADDRESS && log.data == expected),
        "admin rotation must emit the expected event"
    );
}

fn word_from_address(address: Address) -> U256 {
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(address.as_slice());
    U256::from_be_slice(&word)
}
