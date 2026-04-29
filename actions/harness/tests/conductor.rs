//! Action tests for conductor-gated sequencer leadership.

use std::sync::Arc;

use base_action_harness::{
    ActionTestHarness, L2SequencerError, SharedL1Chain, TestConductorHandle,
};

/// Verify that a sequencer wired to a conductor emits exactly one
/// `commit_unsafe_payload` call per block it builds.
#[tokio::test]
async fn conductor_leader_commits_each_block() {
    let h = ActionTestHarness::default();
    let chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());

    let handle = TestConductorHandle::with_leader_zero();
    let mut seq = h.create_l2_sequencer(chain);
    seq.set_conductor(Arc::new(handle.conductor(0)));

    for i in 1u64..=3 {
        seq.build_next_block_with_single_transaction().await;
        assert_eq!(
            handle.committed_count(),
            i as usize,
            "expected {i} commits after building block {i}"
        );
    }

    let payloads = handle.committed_payloads();
    assert_eq!(payloads.len(), 3);
    assert!(
        payloads.iter().all(|(id, _)| *id == 0),
        "all commits should be attributed to sequencer 0"
    );
    // Each committed block hash must be distinct.
    let hashes: std::collections::HashSet<_> = payloads.iter().map(|(_, h)| h).collect();
    assert_eq!(hashes.len(), 3, "committed hashes must all be distinct");
}

/// Verify that a sequencer with no conductor leadership cannot build blocks.
#[tokio::test]
async fn conductor_non_leader_cannot_build() {
    let h = ActionTestHarness::default();
    let chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());

    // Sequencer id=1 but only id=0 is leader.
    let handle = TestConductorHandle::with_leader_zero();
    let mut seq = h.create_l2_sequencer(chain);
    seq.set_conductor(Arc::new(handle.conductor(1)));

    let err = seq.try_build_next_block_with_transactions(vec![]).await.unwrap_err();
    assert!(
        matches!(err, L2SequencerError::NotLeader),
        "non-leader sequencer must return NotLeader, got: {err}"
    );
    assert_eq!(handle.committed_count(), 0, "no payloads should be committed");
}

/// Verify that leadership transfer correctly gates block production: the old
/// leader is blocked after transfer and the new leader can build.
#[tokio::test]
async fn conductor_leadership_transfer_gates_building() {
    let h = ActionTestHarness::default();
    let chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());

    let handle = TestConductorHandle::with_leader_zero();
    let mut seq = h.create_l2_sequencer(chain);
    seq.set_conductor(Arc::new(handle.conductor(0)));

    // Build 2 blocks as leader.
    seq.build_next_block_with_single_transaction().await;
    seq.build_next_block_with_single_transaction().await;
    assert_eq!(handle.committed_count(), 2);

    // Revoke leadership.
    handle.clear_leader();

    let err = seq.try_build_next_block_with_transactions(vec![]).await.unwrap_err();
    assert!(
        matches!(err, L2SequencerError::NotLeader),
        "sequencer must be blocked after leadership revoked, got: {err}"
    );
    assert_eq!(handle.committed_count(), 2, "no new commits after revocation");

    // Restore leadership.
    handle.set_leader(0);
    seq.build_next_block_with_single_transaction().await;
    assert_eq!(handle.committed_count(), 3, "commit should succeed after leadership restored");
}

/// Verify that two independent sequencers sharing a [`TestConductorHandle`]
/// obey its leadership assignment: only the designated leader commits payloads,
/// and payloads from each sequencer are attributed to the correct id.
#[tokio::test]
async fn conductor_two_sequencers_only_leader_produces() {
    let h = ActionTestHarness::default();

    // Sequencer A starts as leader (id=0); sequencer B (id=1) is a follower.
    let handle = TestConductorHandle::with_leader_zero();

    let chain_a = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut seq_a = h.create_l2_sequencer(chain_a);
    seq_a.set_conductor(Arc::new(handle.conductor(0)));

    let chain_b = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut seq_b = h.create_l2_sequencer(chain_b);
    seq_b.set_conductor(Arc::new(handle.conductor(1)));

    // Sequencer B is blocked.
    assert!(matches!(
        seq_b.try_build_next_block_with_transactions(vec![]).await.unwrap_err(),
        L2SequencerError::NotLeader
    ));

    // Sequencer A builds 3 blocks.
    for _ in 0..3 {
        seq_a.build_next_block_with_single_transaction().await;
    }
    assert_eq!(handle.committed_by(0).len(), 3, "sequencer A must have 3 commits");
    assert_eq!(handle.committed_by(1).len(), 0, "sequencer B must have 0 commits");

    // Transfer leadership to B.
    handle.set_leader(1);

    // Sequencer A is now blocked.
    assert!(matches!(
        seq_a.try_build_next_block_with_transactions(vec![]).await.unwrap_err(),
        L2SequencerError::NotLeader
    ));

    // Sequencer B builds 2 blocks.
    for _ in 0..2 {
        seq_b.build_next_block_with_single_transaction().await;
    }
    assert_eq!(handle.committed_by(0).len(), 3, "sequencer A commits must not grow");
    assert_eq!(handle.committed_by(1).len(), 2, "sequencer B must have 2 commits");
    assert_eq!(handle.committed_count(), 5, "total commits across both sequencers");
}
