//! B-20 precompile action tests across the Base Beryl boundary.

use alloy_primitives::U256;

use crate::env::BerylTestEnv;

#[tokio::test]
async fn beryl_enables_b20_factory_and_dynamic_token_precompile() {
    let mut env = BerylTestEnv::new();
    let token = env.b20_token_address();

    let (total_supply_probe, deploy_total_supply_probe) = env.deploy_staticcall_probe_tx(token);
    let (alice_balance_probe, deploy_alice_balance_probe) = env.deploy_staticcall_probe_tx(token);
    let (bob_balance_probe, deploy_bob_balance_probe) = env.deploy_staticcall_probe_tx(token);
    let (carol_balance_probe, deploy_carol_balance_probe) = env.deploy_staticcall_probe_tx(token);
    let (allowance_probe, deploy_allowance_probe) = env.deploy_staticcall_probe_tx(token);
    let (decimals_probe, deploy_decimals_probe) = env.deploy_staticcall_probe_tx(token);

    let pre_beryl_create = env.create_b20_token_tx();
    let block1 = env
        .sequencer
        .build_next_block_with_transactions(vec![
            deploy_total_supply_probe,
            deploy_alice_balance_probe,
            deploy_bob_balance_probe,
            deploy_carol_balance_probe,
            deploy_allowance_probe,
            deploy_decimals_probe,
            pre_beryl_create,
        ])
        .await;

    assert!(!env.sequencer.has_code(token), "B-20 token code must not be deployed before Beryl");
    assert_eq!(
        env.b20_total_supply(token),
        U256::ZERO,
        "B-20 total supply must remain unset before Beryl"
    );

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

    let transfer_to_bob =
        env.transfer_b20_tx(token, BerylTestEnv::bob(), U256::from(BerylTestEnv::B20_BOB_TRANSFER));
    let block4 = env.sequencer.build_next_block_with_transactions(vec![transfer_to_bob]).await;

    assert!(env.user_tx_succeeded(&block4, 0), "Alice transfer transaction must succeed");
    assert!(
        env.b20_transfer_log_emitted(
            &block4,
            0,
            token,
            BerylTestEnv::alice(),
            BerylTestEnv::bob(),
            U256::from(BerylTestEnv::B20_BOB_TRANSFER),
        ),
        "Alice transfer must emit a Transfer event"
    );
    assert_eq!(
        env.b20_balance(token, BerylTestEnv::alice()),
        U256::from(BerylTestEnv::B20_INITIAL_SUPPLY - BerylTestEnv::B20_BOB_TRANSFER),
        "Alice balance must decrease after transferring to Bob"
    );
    assert_eq!(
        env.b20_balance(token, BerylTestEnv::bob()),
        U256::from(BerylTestEnv::B20_BOB_TRANSFER),
        "Bob balance must increase after receiving B-20"
    );
    assert_eq!(
        env.b20_total_supply(token),
        U256::from(BerylTestEnv::B20_INITIAL_SUPPLY),
        "B-20 total supply must not change after transfer"
    );

    let bob_transfer_to_carol = env.transfer_b20_from_bob_tx(
        token,
        BerylTestEnv::carol(),
        U256::from(BerylTestEnv::B20_CAROL_TRANSFER),
    );
    let block5 =
        env.sequencer.build_next_block_with_transactions(vec![bob_transfer_to_carol]).await;

    assert!(env.user_tx_succeeded(&block5, 0), "Bob transfer transaction must succeed");
    assert!(
        env.b20_transfer_log_emitted(
            &block5,
            0,
            token,
            BerylTestEnv::bob(),
            BerylTestEnv::carol(),
            U256::from(BerylTestEnv::B20_CAROL_TRANSFER),
        ),
        "Bob transfer must emit a Transfer event"
    );
    assert_eq!(
        env.b20_balance(token, BerylTestEnv::alice()),
        U256::from(BerylTestEnv::B20_INITIAL_SUPPLY - BerylTestEnv::B20_BOB_TRANSFER),
        "Alice balance must remain unchanged after Bob transfers to Carol"
    );
    assert_eq!(
        env.b20_balance(token, BerylTestEnv::bob()),
        U256::from(BerylTestEnv::B20_BOB_TRANSFER - BerylTestEnv::B20_CAROL_TRANSFER),
        "Bob balance must decrease after transferring to Carol"
    );
    assert_eq!(
        env.b20_balance(token, BerylTestEnv::carol()),
        U256::from(BerylTestEnv::B20_CAROL_TRANSFER),
        "Carol balance must increase after receiving B-20"
    );
    assert_eq!(
        env.b20_total_supply(token),
        U256::from(BerylTestEnv::B20_INITIAL_SUPPLY),
        "B-20 total supply must remain constant after multiple transfers"
    );

    let bob_remaining = BerylTestEnv::B20_BOB_TRANSFER - BerylTestEnv::B20_CAROL_TRANSFER;
    let bob_overdraw =
        env.transfer_b20_from_bob_tx(token, BerylTestEnv::carol(), U256::from(bob_remaining + 1));
    let block6 = env.sequencer.build_next_block_with_transactions(vec![bob_overdraw]).await;

    assert!(!env.user_tx_succeeded(&block6, 0), "Bob overdraw transfer must revert");
    assert!(
        !env.b20_transfer_log_emitted(
            &block6,
            0,
            token,
            BerylTestEnv::bob(),
            BerylTestEnv::carol(),
            U256::from(bob_remaining + 1),
        ),
        "failed overdraw transfer must not emit a Transfer event"
    );
    assert_eq!(
        env.b20_balance(token, BerylTestEnv::bob()),
        U256::from(bob_remaining),
        "failed overdraw transfer must leave Bob's balance unchanged"
    );
    assert_eq!(
        env.b20_balance(token, BerylTestEnv::carol()),
        U256::from(BerylTestEnv::B20_CAROL_TRANSFER),
        "failed overdraw transfer must leave Carol's balance unchanged"
    );

    let approve_bob =
        env.approve_b20_tx(token, BerylTestEnv::bob(), U256::from(BerylTestEnv::B20_BOB_ALLOWANCE));
    let block7 = env.sequencer.build_next_block_with_transactions(vec![approve_bob]).await;

    assert!(env.user_tx_succeeded(&block7, 0), "Alice approval transaction must succeed");
    assert!(
        env.b20_approval_log_emitted(
            &block7,
            0,
            token,
            BerylTestEnv::alice(),
            BerylTestEnv::bob(),
            U256::from(BerylTestEnv::B20_BOB_ALLOWANCE),
        ),
        "Alice approval must emit an Approval event"
    );
    assert_eq!(
        env.b20_allowance(token, BerylTestEnv::alice(), BerylTestEnv::bob()),
        U256::from(BerylTestEnv::B20_BOB_ALLOWANCE),
        "Alice must approve Bob's B-20 allowance"
    );

    let transfer_from_alice_to_carol = env.transfer_b20_from_alice_by_bob_tx(
        token,
        BerylTestEnv::carol(),
        U256::from(BerylTestEnv::B20_TRANSFER_FROM_CAROL),
    );
    let block8 =
        env.sequencer.build_next_block_with_transactions(vec![transfer_from_alice_to_carol]).await;

    assert!(env.user_tx_succeeded(&block8, 0), "Bob transferFrom transaction must succeed");
    assert!(
        env.b20_transfer_log_emitted(
            &block8,
            0,
            token,
            BerylTestEnv::alice(),
            BerylTestEnv::carol(),
            U256::from(BerylTestEnv::B20_TRANSFER_FROM_CAROL),
        ),
        "transferFrom must emit a Transfer event from Alice to Carol"
    );

    let alice_final = BerylTestEnv::B20_INITIAL_SUPPLY
        - BerylTestEnv::B20_BOB_TRANSFER
        - BerylTestEnv::B20_TRANSFER_FROM_CAROL;
    let carol_final = BerylTestEnv::B20_CAROL_TRANSFER + BerylTestEnv::B20_TRANSFER_FROM_CAROL;
    let allowance_final = BerylTestEnv::B20_BOB_ALLOWANCE - BerylTestEnv::B20_TRANSFER_FROM_CAROL;

    assert_eq!(
        env.b20_balance(token, BerylTestEnv::alice()),
        U256::from(alice_final),
        "transferFrom must decrease Alice's balance"
    );
    assert_eq!(
        env.b20_balance(token, BerylTestEnv::bob()),
        U256::from(bob_remaining),
        "transferFrom must not change Bob's balance"
    );
    assert_eq!(
        env.b20_balance(token, BerylTestEnv::carol()),
        U256::from(carol_final),
        "transferFrom must increase Carol's balance"
    );
    assert_eq!(
        env.b20_allowance(token, BerylTestEnv::alice(), BerylTestEnv::bob()),
        U256::from(allowance_final),
        "transferFrom must decrement Bob's allowance"
    );
    assert_eq!(
        env.b20_total_supply(token),
        U256::from(BerylTestEnv::B20_INITIAL_SUPPLY),
        "B-20 total supply must remain constant after transferFrom"
    );

    let block9 = env
        .sequencer
        .build_next_block_with_transactions(vec![
            env.probe_b20_total_supply_tx(total_supply_probe),
            env.probe_b20_balance_tx(alice_balance_probe, BerylTestEnv::alice()),
            env.probe_b20_balance_tx(bob_balance_probe, BerylTestEnv::bob()),
            env.probe_b20_balance_tx(carol_balance_probe, BerylTestEnv::carol()),
            env.probe_b20_allowance_tx(allowance_probe, BerylTestEnv::alice(), BerylTestEnv::bob()),
            env.probe_b20_decimals_tx(decimals_probe),
        ])
        .await;

    assert!(env.probe_call_succeeded(total_supply_probe), "totalSupply ABI call must succeed");
    assert_eq!(
        env.probe_return_word(total_supply_probe),
        U256::from(BerylTestEnv::B20_INITIAL_SUPPLY),
        "totalSupply ABI call must return the initialized supply"
    );
    assert!(env.probe_call_succeeded(alice_balance_probe), "Alice balanceOf ABI call must succeed");
    assert_eq!(
        env.probe_return_word(alice_balance_probe),
        U256::from(alice_final),
        "Alice balanceOf ABI call must match storage"
    );
    assert!(env.probe_call_succeeded(bob_balance_probe), "Bob balanceOf ABI call must succeed");
    assert_eq!(
        env.probe_return_word(bob_balance_probe),
        U256::from(bob_remaining),
        "Bob balanceOf ABI call must match storage"
    );
    assert!(env.probe_call_succeeded(carol_balance_probe), "Carol balanceOf ABI call must succeed");
    assert_eq!(
        env.probe_return_word(carol_balance_probe),
        U256::from(carol_final),
        "Carol balanceOf ABI call must match storage"
    );
    assert!(env.probe_call_succeeded(allowance_probe), "allowance ABI call must succeed");
    assert_eq!(
        env.probe_return_word(allowance_probe),
        U256::from(allowance_final),
        "allowance ABI call must match storage"
    );
    assert!(env.probe_call_succeeded(decimals_probe), "decimals ABI call must succeed");
    assert_eq!(
        env.probe_return_word(decimals_probe),
        U256::from(BerylTestEnv::B20_DECIMALS),
        "decimals ABI call must return the token-address encoded decimals"
    );

    env.derive_blocks(
        [
            (block1, 1),
            (block2, 2),
            (block3, 3),
            (block4, 4),
            (block5, 5),
            (block6, 6),
            (block7, 7),
            (block8, 8),
            (block9, 9),
        ],
        9,
    )
    .await;
}
