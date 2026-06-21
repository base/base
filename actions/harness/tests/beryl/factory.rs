//! B-20 factory precompile action tests across the Base Beryl boundary.

use alloy_consensus::TxReceipt;
use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_sol_types::{SolCall, SolEvent, SolValue};
use base_common_consensus::BaseBlock;
use base_common_precompiles::{B20FactoryStorage, B20Variant, IB20Factory};

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
async fn b20_creation_reverts_while_variant_feature_is_deactivated() {
    let mut env = BerylTestEnv::new();

    let block1 = env.sequencer.build_empty_block().await;
    let activation_block = B20FactoryPrecompiles::activate(&mut env).await;

    let create_first_token = env.create_b20_token_tx();
    let block2 = env.sequencer.build_next_block_with_transactions(vec![create_first_token]).await;
    assert!(env.user_tx_succeeded(&block2, 0), "first B-20 creation must succeed");

    let deactivate_b20_asset = env.deactivate_feature_tx(BerylTestEnv::b20_asset_feature());
    let block3 = env.sequencer.build_next_block_with_transactions(vec![deactivate_b20_asset]).await;
    assert!(env.user_tx_succeeded(&block3, 0), "B20_ASSET deactivation must succeed");

    let create_while_deactivated = env.create_b20_token_with_salt_tx(BerylTestEnv::ALT_SALT);
    let block4 =
        env.sequencer.build_next_block_with_transactions(vec![create_while_deactivated]).await;
    assert!(
        !env.user_tx_succeeded(&block4, 0),
        "B-20 creation must revert when B20_ASSET variant is deactivated"
    );

    let reactivate_b20_asset = env.activate_feature_tx(BerylTestEnv::b20_asset_feature());
    let block5 = env.sequencer.build_next_block_with_transactions(vec![reactivate_b20_asset]).await;
    assert!(env.user_tx_succeeded(&block5, 0), "B20_ASSET re-activation must succeed");

    let create_after_reactivate = env.create_b20_token_with_salt_tx(BerylTestEnv::ALT_SALT);
    let block6 =
        env.sequencer.build_next_block_with_transactions(vec![create_after_reactivate]).await;
    assert!(
        env.user_tx_succeeded(&block6, 0),
        "B-20 creation must succeed after B20_ASSET variant is re-activated"
    );

    env.derive_blocks(
        [
            (block1, 1),
            (activation_block, 2),
            (block2, 3),
            (block3, 4),
            (block4, 5),
            (block5, 6),
            (block6, 7),
        ],
        7,
    )
    .await;
}

#[tokio::test]
async fn b20_factory_views_and_events_are_available_after_beryl_activation() {
    let mut env = BerylTestEnv::new();
    let token = env.b20_token_address();

    let block1 = env.sequencer.build_empty_block().await;
    let activation_block = B20FactoryPrecompiles::activate(&mut env).await;

    let create = env.create_b20_token_tx();
    let block2 = env.sequencer.build_next_block_with_transactions(vec![create]).await;

    assert!(env.user_tx_succeeded(&block2, 0), "B-20 creation transaction must succeed");
    assert_token_created_log(&env, &block2, token);

    let (probe, deploy_probe) = env.deploy_staticcall_probe_tx(B20FactoryStorage::ADDRESS);
    let block3 = env.sequencer.build_next_block_with_transactions(vec![deploy_probe]).await;
    assert!(env.user_tx_succeeded(&block3, 0), "factory staticcall probe must deploy");

    let get_token_address = env.call_staticcall_probe_tx(
        probe,
        Bytes::from(
            IB20Factory::getB20AddressCall {
                variant: IB20Factory::B20Variant::ASSET,
                sender: BerylTestEnv::alice(),
                salt: BerylTestEnv::b20_token_salt(),
            }
            .abi_encode(),
        ),
        BerylTestEnv::B20_PROBE_GAS_LIMIT,
    );
    let block4 = env.sequencer.build_next_block_with_transactions(vec![get_token_address]).await;

    assert!(env.probe_call_succeeded(probe), "getB20Address() staticcall must succeed");
    assert_eq!(
        env.probe_return_word(probe),
        word_from_address(token),
        "getB20Address() must return the deterministic token address"
    );

    let is_b20 = env.call_staticcall_probe_tx(
        probe,
        Bytes::from(IB20Factory::isB20Call { token }.abi_encode()),
        BerylTestEnv::B20_PROBE_GAS_LIMIT,
    );
    let block5 = env.sequencer.build_next_block_with_transactions(vec![is_b20]).await;

    assert!(env.probe_call_succeeded(probe), "isB20() staticcall must succeed");
    assert_eq!(env.probe_return_word(probe), U256::ONE, "created token must be B-20");

    let is_not_b20 = env.call_staticcall_probe_tx(
        probe,
        Bytes::from(IB20Factory::isB20Call { token: B20FactoryStorage::ADDRESS }.abi_encode()),
        BerylTestEnv::B20_PROBE_GAS_LIMIT,
    );
    let block6 = env.sequencer.build_next_block_with_transactions(vec![is_not_b20]).await;

    assert!(env.probe_call_succeeded(probe), "isB20(non-token) staticcall must succeed");
    assert_eq!(env.probe_return_word(probe), U256::ZERO, "factory singleton must not be B-20");

    let is_initialized = env.call_staticcall_probe_tx(
        probe,
        Bytes::from(IB20Factory::isB20InitializedCall { token }.abi_encode()),
        BerylTestEnv::B20_PROBE_GAS_LIMIT,
    );
    let block7 = env.sequencer.build_next_block_with_transactions(vec![is_initialized]).await;

    assert!(env.probe_call_succeeded(probe), "isB20Initialized() staticcall must succeed");
    assert_eq!(env.probe_return_word(probe), U256::ONE, "created token must be initialized");

    let is_not_initialized = env.call_staticcall_probe_tx(
        probe,
        Bytes::from(
            IB20Factory::isB20InitializedCall { token: Address::repeat_byte(0xab) }.abi_encode(),
        ),
        BerylTestEnv::B20_PROBE_GAS_LIMIT,
    );
    let block8 = env.sequencer.build_next_block_with_transactions(vec![is_not_initialized]).await;

    assert!(env.probe_call_succeeded(probe), "isB20Initialized(non-token) staticcall must succeed");
    assert_eq!(env.probe_return_word(probe), U256::ZERO, "non-token must not be initialized");

    let malformed_stablecoin_create = env.create_tx(
        TxKind::Call(B20FactoryStorage::ADDRESS),
        Bytes::from(
            IB20Factory::createB20Call {
                variant: IB20Factory::B20Variant::STABLECOIN,
                salt: BerylTestEnv::ALT_SALT,
                params: Bytes::new(),
                initCalls: Vec::new(),
            }
            .abi_encode(),
        ),
        BerylTestEnv::B20_GAS_LIMIT,
    );
    let block9 =
        env.sequencer.build_next_block_with_transactions(vec![malformed_stablecoin_create]).await;

    assert!(!env.user_tx_succeeded(&block9, 0), "malformed stablecoin params must revert");

    env.derive_blocks(
        [
            (block1, 1),
            (activation_block, 2),
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
async fn b20_factory_rejects_invalid_creation_parameters() {
    let mut env = BerylTestEnv::new();

    let block1 = env.sequencer.build_empty_block().await;
    let activate_asset = env.activate_feature_tx(BerylTestEnv::b20_asset_feature());
    let activate_stablecoin = env.activate_feature_tx(BerylTestEnv::b20_stablecoin_feature());
    let block2 = env
        .sequencer
        .build_next_block_with_transactions(vec![activate_asset, activate_stablecoin])
        .await;
    assert!(env.user_tx_succeeded(&block2, 0), "B20_ASSET activation must succeed");
    assert!(env.user_tx_succeeded(&block2, 1), "B20_STABLECOIN activation must succeed");

    let invalid_variant = env.create_tx(
        TxKind::Call(B20FactoryStorage::ADDRESS),
        Bytes::from(
            IB20Factory::createB20Call {
                variant: IB20Factory::B20Variant::__Invalid,
                salt: BerylTestEnv::ALT_SALT,
                params: Bytes::new(),
                initCalls: Vec::new(),
            }
            .abi_encode(),
        ),
        BerylTestEnv::B20_GAS_LIMIT,
    );
    let unsupported_asset_version = env.create_tx(
        TxKind::Call(B20FactoryStorage::ADDRESS),
        Bytes::from(
            IB20Factory::createB20Call {
                variant: IB20Factory::B20Variant::ASSET,
                salt: BerylTestEnv::ALT_SALT,
                params: IB20Factory::B20AssetCreateParams {
                    version: B20Variant::Asset.supported_version() + 1,
                    name: "Bad Version".to_string(),
                    symbol: "BADV".to_string(),
                    initialAdmin: BerylTestEnv::alice(),
                    decimals: BerylTestEnv::B20_DECIMALS,
                }
                .abi_encode()
                .into(),
                initCalls: Vec::new(),
            }
            .abi_encode(),
        ),
        BerylTestEnv::B20_GAS_LIMIT,
    );
    let invalid_asset_decimals = env.create_tx(
        TxKind::Call(B20FactoryStorage::ADDRESS),
        Bytes::from(
            IB20Factory::createB20Call {
                variant: IB20Factory::B20Variant::ASSET,
                salt: BerylTestEnv::ALT_SALT,
                params: IB20Factory::B20AssetCreateParams {
                    version: B20Variant::Asset.supported_version(),
                    name: "Bad Decimals".to_string(),
                    symbol: "BADD".to_string(),
                    initialAdmin: BerylTestEnv::alice(),
                    decimals: 5,
                }
                .abi_encode()
                .into(),
                initCalls: Vec::new(),
            }
            .abi_encode(),
        ),
        BerylTestEnv::B20_GAS_LIMIT,
    );
    let missing_currency = env.create_tx(
        TxKind::Call(B20FactoryStorage::ADDRESS),
        Bytes::from(
            IB20Factory::createB20Call {
                variant: IB20Factory::B20Variant::STABLECOIN,
                salt: BerylTestEnv::ALT_SALT,
                params: IB20Factory::B20StablecoinCreateParams {
                    version: B20Variant::Stablecoin.supported_version(),
                    name: "Missing Currency".to_string(),
                    symbol: "MISS".to_string(),
                    initialAdmin: BerylTestEnv::alice(),
                    currency: String::new(),
                }
                .abi_encode()
                .into(),
                initCalls: Vec::new(),
            }
            .abi_encode(),
        ),
        BerylTestEnv::B20_GAS_LIMIT,
    );
    let block3 = env
        .sequencer
        .build_next_block_with_transactions(vec![
            invalid_variant,
            unsupported_asset_version,
            invalid_asset_decimals,
            missing_currency,
        ])
        .await;

    for index in 0..4 {
        assert!(!env.user_tx_succeeded(&block3, index), "invalid factory call {index} must revert");
    }
    let invalid_asset =
        B20Variant::Asset.compute_address(BerylTestEnv::alice(), BerylTestEnv::ALT_SALT).0;
    let invalid_stablecoin =
        B20Variant::Stablecoin.compute_address(BerylTestEnv::alice(), BerylTestEnv::ALT_SALT).0;
    assert!(!env.sequencer.has_code(invalid_asset), "invalid asset creations must not deploy code");
    assert!(
        !env.sequencer.has_code(invalid_stablecoin),
        "invalid stablecoin creation must not deploy code"
    );

    let (probe, deploy_probe) = env.deploy_staticcall_probe_tx(B20FactoryStorage::ADDRESS);
    let block4 = env.sequencer.build_next_block_with_transactions(vec![deploy_probe]).await;
    assert!(env.user_tx_succeeded(&block4, 0), "factory staticcall probe must deploy");

    let invalid_address = env.call_staticcall_probe_tx(
        probe,
        Bytes::from(
            IB20Factory::getB20AddressCall {
                variant: IB20Factory::B20Variant::__Invalid,
                sender: BerylTestEnv::alice(),
                salt: BerylTestEnv::ALT_SALT,
            }
            .abi_encode(),
        ),
        BerylTestEnv::B20_PROBE_GAS_LIMIT,
    );
    let block5 = env.sequencer.build_next_block_with_transactions(vec![invalid_address]).await;
    assert!(env.user_tx_succeeded(&block5, 0), "invalid getB20Address probe tx must succeed");
    assert!(
        !env.probe_call_succeeded(probe),
        "getB20Address(__Invalid) staticcall must revert with strict ABI decoding"
    );

    env.derive_blocks([(block1, 1), (block2, 2), (block3, 3), (block4, 4), (block5, 5)], 5).await;
}

struct B20FactoryPrecompiles;

impl B20FactoryPrecompiles {
    async fn activate(env: &mut BerylTestEnv) -> BaseBlock {
        let activate_b20 = env.activate_feature_tx(BerylTestEnv::b20_asset_feature());
        let block = env.sequencer.build_next_block_with_transactions(vec![activate_b20]).await;

        assert!(env.user_tx_succeeded(&block, 0), "B20_ASSET activation must succeed");

        block
    }
}

fn assert_token_created_log(env: &BerylTestEnv, block: &BaseBlock, token: Address) {
    let expected = IB20Factory::B20Created {
        token,
        variant: IB20Factory::B20Variant::ASSET,
        name: BerylTestEnv::B20_NAME.to_string(),
        symbol: BerylTestEnv::B20_SYMBOL.to_string(),
        decimals: BerylTestEnv::B20_DECIMALS,
        variantParams: Bytes::new(),
    }
    .encode_log_data();
    assert!(
        env.user_tx_receipt(block, 0)
            .logs()
            .iter()
            .any(|log| log.address == B20FactoryStorage::ADDRESS && log.data == expected),
        "createB20() must emit B20Created"
    );
}

fn word_from_address(address: Address) -> U256 {
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(address.as_slice());
    U256::from_be_slice(&word)
}
