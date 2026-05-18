//! Production-mode L1 data-availability action tests.

use alloy_consensus::{Transaction, transaction::SignerRecoverable};
use alloy_primitives::B256;
use alloy_signer_local::PrivateKeySigner;
use base_action_harness::{
    ActionBlobProvider, ActionL1ChainProvider, ActionL2Source, ActionTestHarness, Batcher,
    BatcherConfig, L1MinerConfig, SharedL1Chain, TestRollupConfigBuilder, block_info_from,
};
use base_batcher_encoder::{DaType, EncoderConfig};
use base_consensus_derive::{DataAvailabilityProvider, EthereumDataSource};

fn test_l1_signer() -> PrivateKeySigner {
    PrivateKeySigner::from_bytes(&B256::repeat_byte(0x11)).expect("valid test signer")
}

#[tokio::test]
async fn production_da_calldata_uses_signed_l1_transactions() {
    let signer = test_l1_signer();
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    }
    .with_l1_signer(signer.clone());
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let mut source = ActionL2Source::new();
    source.push(sequencer.build_next_block_with_single_transaction().await);

    Batcher::new(source, &h.rollup_config, batcher_cfg.clone()).advance(&mut h.l1).await;

    let block = h.l1.tip();
    assert_eq!(block.transactions.len(), 1);
    let tx = &block.transactions[0];
    assert!(tx.is_eip1559());
    assert_eq!(tx.recover_signer().expect("signed tx recovers"), signer.address());
    assert_eq!(tx.to(), Some(batcher_cfg.inbox_address));

    let mut pre_ecotone_cfg = h.rollup_config.clone();
    pre_ecotone_cfg.hardforks.ecotone_time = None;
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut dap = EthereumDataSource::new_from_parts(
        ActionL1ChainProvider::new(l1_chain.clone()),
        ActionBlobProvider::new(l1_chain),
        &pre_ecotone_cfg,
    );
    let calldata = dap
        .next(&block_info_from(block), signer.address())
        .await
        .expect("production calldata source reads signed tx input");
    assert_eq!(calldata.as_ref(), tx.input().as_ref());

    let (mut node, _chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;
    assert_eq!(node.run_until_idle().await, 1);
    assert_eq!(node.l2_safe_number(), 1);
}

#[tokio::test]
async fn production_da_blobs_link_versioned_hashes_to_sidecars() {
    let signer = test_l1_signer();
    let batcher_cfg = BatcherConfig::default().with_l1_signer(signer.clone());
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);
    let mut source = ActionL2Source::new();
    source.push(sequencer.build_next_block_with_single_transaction().await);

    Batcher::new(source, &h.rollup_config, batcher_cfg).advance(&mut h.l1).await;

    let block = h.l1.tip();
    assert_eq!(block.transactions.len(), 1);
    assert_eq!(block.blob_sidecars.len(), 1);
    let tx = &block.transactions[0];
    assert!(tx.is_eip4844());
    assert_eq!(tx.recover_signer().expect("signed tx recovers"), signer.address());
    let hashes = tx.blob_versioned_hashes().expect("blob tx has versioned hashes");
    assert_eq!(hashes, &[block.blob_sidecars[0].0]);

    let (mut node, _chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;
    assert_eq!(node.run_until_idle().await, 1);
    assert_eq!(node.l2_safe_number(), 1);
}
