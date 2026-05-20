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

    let post_beryl_probe = env.create_tx(TxKind::Call(probe), hello_world, GAS_LIMIT);
    let block2 = env.sequencer.build_next_block_with_transactions(vec![post_beryl_probe]).await;

    assert_eq!(
        env.sequencer.storage_at(probe, CALL_SUCCESS_SLOT),
        U256::from(1),
        "policy registry staticcall must succeed after Beryl"
    );
    assert_eq!(
        env.sequencer.storage_at(probe, RETURN_WORD_SLOT),
        U256::from(1),
        "policy registry helloWorld must return true after Beryl"
    );

    env.derive_blocks([(block1, 1), (block2, 2)], 2).await;
}
