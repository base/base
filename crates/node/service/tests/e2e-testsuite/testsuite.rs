use std::sync::Arc;

use alloy_primitives::{Address, B64, B256};
use base_common_chain_config::{BaseChainSpec, BaseChainSpecBuilder};
use base_common_types_payload::BasePayloadAttributes;
use base_execution_payload_builder::BasePayloadBuilderAttributes;
use base_testing_devnet::{
    BaseNodeTestUtils, testsuite::TestBuilder, testsuite::actions::AssertMineBlock,
    testsuite::setup::NetworkSetup, testsuite::setup::Setup,
};
use eyre::Result;

#[tokio::test]
async fn test_testsuite_op_assert_mine_block() -> Result<()> {
    base_common_observability_tracing::init_test_tracing();

    let setup = Setup::default()
        .with_chain_spec(Arc::new(
            BaseChainSpecBuilder::default()
                .chain(BaseChainSpec::mainnet().chain())
                .genesis(BaseNodeTestUtils::genesis())
                .build(),
        ))
        .with_network(NetworkSetup::single_node());

    let test = TestBuilder::new().with_setup(setup).with_action(AssertMineBlock::new(
        0,
        vec![],
        Some(B256::ZERO),
        // TODO: refactor once we have actions to generate payload attributes.
        BasePayloadBuilderAttributes::try_new(
            B256::ZERO,
            BasePayloadAttributes {
                payload_attributes: base_common_types_payload::PayloadAttributes {
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs(),
                    prev_randao: B256::random(),
                    suggested_fee_recipient: Address::random(),
                    withdrawals: None,
                    parent_beacon_block_root: None,
                    slot_number: None,
                    target_gas_limit: None,
                },
                transactions: None,
                no_tx_pool: None,
                eip_1559_params: None,
                min_base_fee: None,
                gas_limit: Some(30_000_000),
            },
            3,
        )
        .expect("valid test payload attributes"),
    ));

    test.run(BaseNodeTestUtils::test_setup).await?;

    Ok(())
}

#[tokio::test]
async fn test_testsuite_op_assert_mine_block_isthmus_activated() -> Result<()> {
    base_common_observability_tracing::init_test_tracing();

    let setup = Setup::default()
        .with_chain_spec(Arc::new(
            BaseChainSpecBuilder::default()
                .chain(BaseChainSpec::mainnet().chain())
                .genesis(BaseNodeTestUtils::genesis())
                .isthmus_activated()
                .build(),
        ))
        .with_network(NetworkSetup::single_node());

    let test = TestBuilder::new().with_setup(setup).with_action(AssertMineBlock::new(
        0,
        vec![],
        Some(B256::ZERO),
        // TODO: refactor once we have actions to generate payload attributes.
        BasePayloadBuilderAttributes::try_new(
            B256::ZERO,
            BasePayloadAttributes {
                payload_attributes: base_common_types_payload::PayloadAttributes {
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs(),
                    prev_randao: B256::random(),
                    suggested_fee_recipient: Address::random(),
                    withdrawals: Some(vec![]),
                    parent_beacon_block_root: Some(B256::ZERO),
                    slot_number: None,
                    target_gas_limit: None,
                },
                transactions: None,
                no_tx_pool: None,
                eip_1559_params: Some(B64::ZERO),
                min_base_fee: None,
                gas_limit: Some(30_000_000),
            },
            3,
        )
        .expect("valid test payload attributes"),
    ));

    test.run(BaseNodeTestUtils::test_setup).await?;

    Ok(())
}
