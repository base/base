//! Tests local payload timestamps with an isolated process-wide replay clock.

use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use alloy_consensus::Header;
use base_clock::{GlobalClock, OffsetClock};
use base_execution_chainspec::BaseChainSpec;
use base_node_core::BaseLocalPayloadAttributesBuilder;
use reth_node_api::PayloadAttributesBuilder;
use reth_payload_primitives::PayloadAttributes;
use reth_primitives_traits::SealedHeader;

#[test]
fn payload_timestamp_uses_installed_clock_when_parent_is_older() {
    let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    let before = Instant::now();
    GlobalClock::set(OffsetClock::new(start)).unwrap();
    let builder = BaseLocalPayloadAttributesBuilder::new(Arc::new(BaseChainSpec::mainnet()));
    let parent = SealedHeader::seal_slow(Header::default());

    let attributes = builder.build(&parent);

    assert!((1_000..=1_000 + before.elapsed().as_secs()).contains(&attributes.timestamp()));
}
