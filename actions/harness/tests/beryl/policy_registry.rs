//! Policy registry precompile action tests across the Base Beryl boundary.

use alloy_primitives::{Bytes, TxKind, U256, hex};
use alloy_sol_types::SolCall;
use base_common_precompiles::IPolicyRegistry;

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
    let hello_world = Bytes::from(IPolicyRegistry::helloWorldCall {}.abi_encode());

    let deploy_probe = env.create_tx(
        TxKind::Create,
        Bytes::from_static(&POLICY_REGISTRY_PROBE_INIT_CODE),
        GAS_LIMIT,
    );
    let pre_beryl_probe = env.create_tx(TxKind::Call(probe), hello_world.clone(), GAS_LIMIT);
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
    let post_beryl_probe = env.create_tx(TxKind::Call(probe), hello_world.clone(), GAS_LIMIT);
    let block3 = env.sequencer.build_next_block_with_transactions(vec![post_beryl_probe]).await;

    assert_eq!(
        env.sequencer.storage_at(probe, CALL_SUCCESS_SLOT),
        U256::from(1),
        "policy registry staticcall must succeed after activation"
    );
    assert_eq!(
        env.sequencer.storage_at(probe, RETURN_WORD_SLOT),
        U256::from(1),
        "policy registry helloWorld must return true after activation"
    );

    // -- Deactivation tests --
    // Block4: deactivate POLICY_REGISTRY (committed state before block5).
    let deactivate_registry = env.deactivate_feature_tx(BerylTestEnv::policy_registry_feature());
    let block4 = env.sequencer.build_next_block_with_transactions(vec![deactivate_registry]).await;

    assert!(env.user_tx_succeeded(&block4, 0), "POLICY_REGISTRY deactivation must succeed");

    // Block5: probe's staticcall must fail while POLICY_REGISTRY is deactivated.
    let probe_while_deactivated =
        env.create_tx(TxKind::Call(probe), hello_world.clone(), GAS_LIMIT);
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
    let probe_after_reactivate = env.create_tx(TxKind::Call(probe), hello_world, GAS_LIMIT);
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
        "policy registry helloWorld must return true after re-activation"
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
