//! Action tests for unsafe block signing and P2P signature validation.

use alloy_primitives::{B256, Signature, U256};
use alloy_signer_local::PrivateKeySigner;
use base_action_harness::{ActionTestHarness, SharedL1Chain, TestGossipTransport};
use base_common_rpc_types_engine::{BaseExecutionPayload, NetworkPayloadEnvelope, PayloadHash};
use base_consensus_node::GossipTransport as _;

/// End-to-end: a sequencer with a real signing key produces blocks whose
/// signatures the verifier node accepts, advancing its unsafe head.
#[tokio::test]
async fn signed_blocks_accepted_as_unsafe_head() {
    let h = ActionTestHarness::default();
    let chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let key = PrivateKeySigner::random();

    let mut seq = h.create_l2_sequencer(chain.clone());
    let transport = h.create_signing_p2p(&mut seq, key);
    let mut node = h.create_test_rollup_node(&seq, chain, transport);
    node.initialize().await;

    for _ in 0..3 {
        let block = seq.build_next_block_with_single_transaction().await;
        seq.broadcast_unsafe_block(&block);
    }

    node.run_until_idle().await;
    assert_eq!(node.l2_unsafe().block_info.number, 3, "signed blocks must advance unsafe head");
    assert_eq!(node.l2_safe().block_info.number, 0, "safe head stays at genesis without batches");
}

/// A block carrying a zero signature is silently dropped when the transport
/// has an expected signer configured.
#[tokio::test]
async fn zero_signature_dropped_when_signer_configured() {
    let h = ActionTestHarness::default();
    let chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let key = PrivateKeySigner::random();

    let mut seq = h.create_l2_sequencer(chain);

    let (injector, mut transport) = TestGossipTransport::channel();
    seq.set_supervised_p2p(injector);
    transport.set_block_signer(key.address());
    transport.set_chain_id(h.rollup_config.l2_chain_id.id());

    // Build via sequencer (no signing key set — default zero-sig path).
    let block = seq.build_next_block_with_single_transaction().await;
    seq.broadcast_unsafe_block(&block);

    assert!(
        transport.try_next_unsafe_block().is_none(),
        "zero-signature block must be silently dropped when expected signer is configured"
    );
}

/// A block signed with the wrong key is silently dropped by the transport.
#[tokio::test]
async fn wrong_signing_key_rejected() {
    let h = ActionTestHarness::default();
    let chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());

    let correct_key = PrivateKeySigner::random();
    let wrong_key = PrivateKeySigner::random();

    let mut seq = h.create_l2_sequencer(chain);
    seq.set_unsafe_block_signer(wrong_key);

    let (p2p, mut transport) = TestGossipTransport::channel();
    seq.set_supervised_p2p(p2p);
    transport.set_block_signer(correct_key.address());
    transport.set_chain_id(h.rollup_config.l2_chain_id.id());

    let block = seq.build_next_block_with_single_transaction().await;
    seq.broadcast_unsafe_block(&block);

    assert!(
        transport.try_next_unsafe_block().is_none(),
        "block signed with wrong key must be dropped"
    );
}

/// Directly injecting a zero-sig block into a transport with an expected
/// signer configured is rejected. A block signed by the expected key is
/// accepted when injected through the same channel.
#[tokio::test]
async fn injected_forged_block_dropped_real_block_accepted() {
    let h = ActionTestHarness::default();
    let chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let key = PrivateKeySigner::random();

    let mut seq = h.create_l2_sequencer(chain);
    seq.set_unsafe_block_signer(key.clone());

    let (p2p, mut transport) = TestGossipTransport::channel();
    seq.set_supervised_p2p(p2p.clone());
    transport.set_block_signer(key.address());
    transport.set_chain_id(h.rollup_config.l2_chain_id.id());

    // Build a block and get its execution payload.
    let block = seq.build_next_block_with_single_transaction().await;
    let block_hash = block.header.hash_slow();
    let (execution_payload, _) = BaseExecutionPayload::from_block_unchecked(block_hash, &block);

    // Inject a forged (zero-sig) envelope directly via the supervisor channel.
    p2p.send(NetworkPayloadEnvelope {
        payload: execution_payload,
        signature: Signature::new(U256::ZERO, U256::ZERO, false),
        payload_hash: PayloadHash(B256::ZERO),
        parent_beacon_block_root: block.header.parent_beacon_block_root,
    });
    assert!(transport.try_next_unsafe_block().is_none(), "forged block must be dropped");

    // Now broadcast through the sequencer's signed path — should be accepted.
    seq.broadcast_unsafe_block(&block);
    assert!(transport.try_next_unsafe_block().is_some(), "correctly signed block must be accepted");
}

/// After a key rotation, blocks from the old key are rejected and blocks
/// from the new key are accepted.
#[tokio::test]
async fn signer_rotation_gates_acceptance() {
    let h = ActionTestHarness::default();
    let chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());

    let key_a = PrivateKeySigner::random();
    let key_b = PrivateKeySigner::random();

    let mut seq = h.create_l2_sequencer(chain);
    seq.set_unsafe_block_signer(key_a.clone());

    let (p2p, mut transport) = TestGossipTransport::channel();
    seq.set_supervised_p2p(p2p);
    transport.set_block_signer(key_a.address());
    transport.set_chain_id(h.rollup_config.l2_chain_id.id());

    // key_a block, expected key_a → accepted.
    let block1 = seq.build_next_block_with_single_transaction().await;
    seq.broadcast_unsafe_block(&block1);
    assert!(transport.try_next_unsafe_block().is_some(), "key_a block must pass before rotation");

    // Rotate expected signer to key_b; sequencer still signs with key_a.
    transport.set_block_signer(key_b.address());

    let block2 = seq.build_next_block_with_single_transaction().await;
    seq.broadcast_unsafe_block(&block2);
    assert!(
        transport.try_next_unsafe_block().is_none(),
        "key_a block must be rejected after rotation to key_b"
    );

    // Rotate sequencer to key_b.
    seq.set_unsafe_block_signer(key_b);

    let block3 = seq.build_next_block_with_single_transaction().await;
    seq.broadcast_unsafe_block(&block3);
    assert!(transport.try_next_unsafe_block().is_some(), "key_b block must pass after rotation");
}
