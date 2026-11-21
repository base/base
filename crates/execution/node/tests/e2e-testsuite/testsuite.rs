use std::sync::Arc;

use alloy_primitives::{Address, B64, B256};
use base_common_consensus::BaseTxEnvelope;
use base_common_rpc_types_engine::BasePayloadAttributes;
use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
use base_execution_payload_builder::BasePayloadBuilderAttributes;
use base_node_core::{BaseEngineTypes, BaseFullNodeTypes, BaseNode, BaseNodeTypes, RollupArgs};
use eyre::Result;
use reth_node_api::{FullNodeTypes, NodeTypes};
use reth_node_builder::Node;
use reth_e2e_test_utils::testsuite::{
    TestBuilder,
    actions::AssertMineBlock,
    setup::{NetworkSetup, Setup},
};

/// Explicit genesis-launch configuration for the default-constructed testsuite nodes.
#[derive(Debug, Clone, Default)]
struct GenesisLaunchNode;

impl NodeTypes for GenesisLaunchNode {
    type Primitives = <BaseNode as NodeTypes>::Primitives;
    type ChainSpec = <BaseNode as NodeTypes>::ChainSpec;
    type Storage = <BaseNode as NodeTypes>::Storage;
    type Payload = <BaseNode as NodeTypes>::Payload;
}

impl<N> Node<N> for GenesisLaunchNode
where
    N: FullNodeTypes<Types: BaseFullNodeTypes + BaseNodeTypes>,
{
    type ComponentsBuilder = <BaseNode as Node<N>>::ComponentsBuilder;
    type AddOns = <BaseNode as Node<N>>::AddOns;

    /// Constructs components with explicit permission to launch the test chain.
    fn components_builder(&self) -> Self::ComponentsBuilder {
        let mut args = RollupArgs::default();
        args.genesis_start.allow_genesis_start = true;
        BaseNode::new(args).components()
    }

    /// Uses the production RPC add-ons unchanged.
    fn add_ons(&self) -> Self::AddOns {
        <BaseNode as Node<N>>::add_ons(&BaseNode::default())
    }
}

#[tokio::test]
async fn test_testsuite_op_assert_mine_block() -> Result<()> {
    reth_tracing::init_test_tracing();

    let setup = Setup::default()
        .with_chain_spec(Arc::new(
            BaseChainSpecBuilder::default()
                .chain(BaseChainSpec::mainnet().chain)
                .genesis(serde_json::from_str(include_str!("../assets/genesis.json")).unwrap())
                .build()
                .inner,
        ))
        .with_network(NetworkSetup::single_node());

    let test =
        TestBuilder::new().with_setup(setup).with_action(AssertMineBlock::<BaseEngineTypes>::new(
            0,
            vec![],
            Some(B256::ZERO),
            // TODO: refactor once we have actions to generate payload attributes.
            BasePayloadBuilderAttributes::<BaseTxEnvelope>::try_new(
                B256::ZERO,
                BasePayloadAttributes {
                    payload_attributes: alloy_rpc_types_engine::PayloadAttributes {
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

    test.run::<GenesisLaunchNode>().await?;

    Ok(())
}

#[tokio::test]
async fn test_testsuite_op_assert_mine_block_isthmus_activated() -> Result<()> {
    reth_tracing::init_test_tracing();

    let setup = Setup::default()
        .with_chain_spec(Arc::new(
            BaseChainSpecBuilder::default()
                .chain(BaseChainSpec::mainnet().chain)
                .genesis(serde_json::from_str(include_str!("../assets/genesis.json")).unwrap())
                .isthmus_activated()
                .build()
                .inner,
        ))
        .with_network(NetworkSetup::single_node());

    let test =
        TestBuilder::new().with_setup(setup).with_action(AssertMineBlock::<BaseEngineTypes>::new(
            0,
            vec![],
            Some(B256::ZERO),
            // TODO: refactor once we have actions to generate payload attributes.
            BasePayloadBuilderAttributes::<BaseTxEnvelope>::try_new(
                B256::ZERO,
                BasePayloadAttributes {
                    payload_attributes: alloy_rpc_types_engine::PayloadAttributes {
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

    test.run::<GenesisLaunchNode>().await?;

    Ok(())
}
