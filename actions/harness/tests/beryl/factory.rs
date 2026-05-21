//! B-20 factory precompile action tests across the Base Beryl boundary.

use alloy_primitives::U256;
use base_common_consensus::BaseBlock;

use crate::env::BerylTestEnv;

#[tokio::test]
async fn beryl_enables_b20_factory_precompile() {
    let mut env = BerylTestEnv::new();
    let token = env.b20_token_address();

    let pre_beryl_create = env.create_b20_token_tx();
    let block1 = env.sequencer.build_next_block_with_transactions(vec![pre_beryl_create]).await;

    assert!(!env.sequencer.has_code(token), "B-20 token code must not be deployed before Beryl");
    assert_eq!(
        env.b20_total_supply(token),
        U256::ZERO,
        "B-20 total supply must remain unset before Beryl"
    );

    let beryl_boundary = env.sequencer.build_empty_block().await;
    let activation_block = B20FactoryPrecompiles::activate(&mut env).await;

    let post_beryl_create = env.create_b20_token_tx();
    let block2 = env.sequencer.build_next_block_with_transactions(vec![post_beryl_create]).await;

    assert!(env.user_tx_succeeded(&block2, 0), "B-20 creation transaction must succeed");
    assert!(env.sequencer.has_code(token), "B-20 token code must be deployed after Beryl");
    assert_eq!(
        env.b20_total_supply(token),
        U256::from(BerylTestEnv::B20_INITIAL_SUPPLY),
        "B-20 total supply must be initialized after Beryl"
    );
    assert_eq!(
        env.b20_balance(token, BerylTestEnv::alice()),
        U256::from(BerylTestEnv::B20_INITIAL_SUPPLY),
        "Alice must receive the initial B-20 supply"
    );
    assert_eq!(
        env.b20_balance(token, BerylTestEnv::bob()),
        U256::ZERO,
        "Bob must start with no B-20 balance"
    );
    assert_eq!(
        env.b20_balance(token, BerylTestEnv::carol()),
        U256::ZERO,
        "Carol must start with no B-20 balance"
    );

    env.derive_blocks([(block1, 1), (beryl_boundary, 2), (activation_block, 3), (block2, 4)], 4)
        .await;
}

#[tokio::test]
async fn duplicate_b20_creation_reverts() {
    let mut env = BerylTestEnv::new();
    let token = env.b20_token_address();

    let block1 = env.sequencer.build_empty_block().await;
    let activation_block = B20FactoryPrecompiles::activate(&mut env).await;

    let create = env.create_b20_token_tx();
    let block2 = env.sequencer.build_next_block_with_transactions(vec![create]).await;

    assert!(env.user_tx_succeeded(&block2, 0), "B-20 creation transaction must succeed");
    assert!(env.sequencer.has_code(token), "B-20 token code must be deployed");

    let duplicate_create = env.create_b20_token_tx();
    let block3 = env.sequencer.build_next_block_with_transactions(vec![duplicate_create]).await;

    assert!(!env.user_tx_succeeded(&block3, 0), "duplicate B-20 creation must revert");
    assert_eq!(
        env.b20_total_supply(token),
        U256::from(BerylTestEnv::B20_INITIAL_SUPPLY),
        "duplicate B-20 creation must leave total supply unchanged"
    );
    assert_eq!(
        env.b20_balance(token, BerylTestEnv::alice()),
        U256::from(BerylTestEnv::B20_INITIAL_SUPPLY),
        "duplicate B-20 creation must leave Alice's balance unchanged"
    );

    env.derive_blocks([(block1, 1), (activation_block, 2), (block2, 3), (block3, 4)], 4).await;
}

#[tokio::test]
async fn b20_creation_reverts_while_factory_feature_is_deactivated() {
    let mut env = BerylTestEnv::new();

    let block1 = env.sequencer.build_empty_block().await;
    let activation_block = B20FactoryPrecompiles::activate(&mut env).await;

    let deactivate_factory = env.deactivate_feature_tx(BerylTestEnv::token_factory_feature());
    let block2 = env.sequencer.build_next_block_with_transactions(vec![deactivate_factory]).await;

    assert!(env.user_tx_succeeded(&block2, 0), "TOKEN_FACTORY deactivation must succeed");

    let create_while_deactivated = env.create_b20_token_with_salt_tx(BerylTestEnv::ALT_SALT);
    let block3 =
        env.sequencer.build_next_block_with_transactions(vec![create_while_deactivated]).await;

    assert!(
        !env.user_tx_succeeded(&block3, 0),
        "token creation must revert when TOKEN_FACTORY is deactivated"
    );

    let reactivate_factory = env.activate_feature_tx(BerylTestEnv::token_factory_feature());
    let block4 = env.sequencer.build_next_block_with_transactions(vec![reactivate_factory]).await;

    assert!(env.user_tx_succeeded(&block4, 0), "TOKEN_FACTORY re-activation must succeed");

    let create_after_reactivate = env.create_b20_token_with_salt_tx(BerylTestEnv::ALT_SALT);
    let block5 =
        env.sequencer.build_next_block_with_transactions(vec![create_after_reactivate]).await;

    assert!(
        env.user_tx_succeeded(&block5, 0),
        "token creation must succeed after TOKEN_FACTORY is re-activated"
    );

    env.derive_blocks(
        [(block1, 1), (activation_block, 2), (block2, 3), (block3, 4), (block4, 5), (block5, 6)],
        6,
    )
    .await;
}

struct B20FactoryPrecompiles;

impl B20FactoryPrecompiles {
    async fn activate(env: &mut BerylTestEnv) -> BaseBlock {
        let activate_factory = env.activate_feature_tx(BerylTestEnv::token_factory_feature());
        let activate_b20 = env.activate_feature_tx(BerylTestEnv::b20_token_feature());
        let block = env
            .sequencer
            .build_next_block_with_transactions(vec![activate_factory, activate_b20])
            .await;

        assert!(env.user_tx_succeeded(&block, 0), "TOKEN_FACTORY activation must succeed");
        assert!(env.user_tx_succeeded(&block, 1), "B20_TOKEN activation must succeed");

        block
    }
}
