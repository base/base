use std::sync::Arc;

use base_common_genesis::RollupConfig;
use base_consensus_derive::test_utils::TestAttributesBuilder;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    SequencerActor, SequencerSyncMode,
    actors::{
        MockConductor, MockOriginSelector, MockSequencerEngineClient,
        MockUnsafePayloadGossipClient,
        sequencer::{PayloadBuilder, RecoveryModeGuard},
    },
};

// Returns a test SequencerActor with mocks that can be used or overridden.
pub(in crate::actors::sequencer) fn test_actor() -> SequencerActor<
    TestAttributesBuilder,
    MockConductor,
    MockOriginSelector,
    MockSequencerEngineClient,
    MockUnsafePayloadGossipClient,
> {
    // The sender is intentionally dropped, so the channel starts closed.
    // If future tests need to send messages, keep the sender instead of dropping it.
    let (_admin_api_tx, admin_api_rx) = mpsc::channel(20);
    let rollup_config = Arc::new(RollupConfig::default());
    let recovery_mode = RecoveryModeGuard::new(false);
    let engine_client = Arc::new(MockSequencerEngineClient::new());
    SequencerActor {
        admin_api_rx,
        builder: PayloadBuilder {
            attributes_builder: TestAttributesBuilder::default(),
            engine_client: Arc::clone(&engine_client),
            origin_selector: MockOriginSelector::new(),
            recovery_mode: recovery_mode.clone(),
            rollup_config: Arc::clone(&rollup_config),
        },
        cancellation_token: CancellationToken::new(),
        conductor: None,
        engine_client,
        is_active: true,
        sequencer_sync_mode: SequencerSyncMode::Cl,
        shadow_blocks_per_cycle: None,
        shadow_funding: None,
        recovery_mode,
        rollup_config,
        seal_offset: base_protocol::DEFAULT_SEAL_OFFSET,
        unsafe_payload_gossip_client: MockUnsafePayloadGossipClient::new(),
        sealer: None,
        pending_stop: None,
    }
}
